# NexusOS Architecture Audit

**Date:** 2026-09-07 (updated after Phase 5)
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
| ACPI table parsing | **MISSING** | RSDP address is passed but nothing parses it yet |
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
| Local APIC, SMP | **MISSING** | Unblocked now that MMIO can be mapped |
| User mode, processes | **MISSING** | Threads are kernel-only; no ring 3 yet |
| Handles, IPC, syscalls | **MISSING** | |

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
- Four worker threads run to completion with exactly the expected iteration
  count, and their stacks are unmapped and returned when they are reaped.
- A thread that never yields, at equal priority, is preempted: a sleeping
  ticker wakes on schedule five times while the non-yielding thread completes
  around twelve million iterations. Cooperative scheduling would hang here, so
  this is the check that distinguishes real preemption from the appearance of
  it.
- A 1920x1200 status screen renders live uptime, memory, heap, thread and
  context-switch figures, repainted twice a second by its own thread and
  captured by `scripts/screenshot.ps1`.

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

1. **No local APIC, and therefore no SMP.** The PIT drives a single core. This
   was blocked on the virtual memory manager, since the APIC's registers sit
   above RAM and could not be mapped; that block is now gone.
2. **The framebuffer is mapped write-back, not write-combining.** Correct in
   QEMU, slow on real hardware. Needs PAT configuration.
3. **No TLB shootdown.** `invlpg` handles the running core; a second core would
   keep a stale translation. Cannot be written or tested before SMP exists.
4. **Bootloader allocations are over-conservative.** Page tables, the handoff
   block and the kernel image are allocated as `RuntimeServicesData`, which the
   kernel treats as permanently reserved. This wastes on the order of 100 KiB.
5. **VVFAT, not a real disk image.** QEMU synthesises a FAT filesystem from a
   directory. Excellent for iteration, but it means NexusOS has never booted
   from a genuine partition table. A real GPT + FAT32 image builder is needed
   before any hardware test.
6. **No CI.** `scripts/test.ps1` runs everything, but nothing runs it
   automatically.
7. **The heap never shrinks.** It grows on demand and keeps what it takes.
   Acceptable for a kernel of this size; worth revisiting when there are
   long-running workloads.
8. **The kernel binary has no host test harness.** It is `no_main` with its own
   panic handler, so tests written inside it would compile and never run. What
   can be checked statically is checked with const assertions; the rest is
   covered by boot-marker checks, fault injection and screenshots. Logic worth
   unit testing is moved into a library crate instead, which is why the
   allocators live in `nexus-mm`.
9. **Threads are kernel-only.** There is no ring 3, no address-space separation
   and no system-call boundary yet, so "thread" currently means a kernel thread
   and nothing is isolated from anything else.
10. **No ageing in the scheduler.** Strict priority means a busy high-priority
    thread starves everything below it. Deliberate for now, and it needs real
    workloads before it can be tuned honestly.

Resolved since the first audit: the missing IDT (Phase 2), the
non-interrupt-safe spinlock (`IrqSpinLock`, Phase 2), and the unstripped kernel
image on the ESP, which was costing megabytes of boot-time reads for a
hundred-kilobyte load.

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

1. **Local APIC and its timer**, replacing the PIT as the scheduling tick, then
   SMP bring-up from the ACPI MADT. This is the first thing that needs ACPI
   table parsing, which the RSDP has been waiting for since Phase 1.
2. **Ring 3 and the system-call entry path**, which is what turns a kernel
   thread into a process and makes isolation mean anything.
3. **Handles and IPC**, where the capability model starts.
4. **A real GPT + FAT32 disk image**, before any attempt to boot hardware.

See [NEXUSOS_ROADMAP.md](NEXUSOS_ROADMAP.md) for the full sequence.
