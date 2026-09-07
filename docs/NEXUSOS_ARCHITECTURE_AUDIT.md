# NexusOS Architecture Audit

**Date:** 2026-09-07
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
| Spinlock | **DONE** | Not yet interrupt-safe; see §4 |
| Framebuffer drawing | **PARTIAL** | Rectangles and gradients; no text, no compositor |
| GDT / TSS | **MISSING** | Still on the firmware's descriptors |
| IDT / exceptions | **MISSING** | **Any fault is currently a triple fault** |
| APIC, timer, SMP | **MISSING** | |
| Physical memory allocator | **MISSING** | Map is reported but no allocator consumes it |
| Virtual memory manager | **MISSING** | Kernel runs on the loader's tables |
| Kernel heap | **MISSING** | No `alloc` in the kernel yet |
| Processes, threads, scheduler | **MISSING** | |
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
- A 1920×1200 BGRX framebuffer is selected and reported at `0x80000000`.
- The ACPI RSDP is found in the configuration table.
- The 4 MiB kernel ELF is read from the ESP and its three `PT_LOAD` segments
  are loaded into physical memory.
- Page tables are built in 15 pages and `cr3` is switched; the kernel reads back
  the same root the loader installed, which proves the switch took effect.
- The kernel runs at its linked higher-half address, on the guarded boot stack.
- 1017 MiB of usable RAM is classified across 22 regions.
- The framebuffer is painted through the direct map, confirmed by screenshot.

16 host-side unit tests cover UEFI structure offsets, the ELF parser and
memory-map normalization. The build produces zero warnings.

## 4. Known deficiencies

These are real and are tracked, not hidden:

1. **No IDT.** The most serious gap. Until Phase 2 lands, any page fault,
   divide error or invalid opcode escalates to a triple fault and resets the
   machine with no diagnostic. This is the next thing to fix.
2. **`SpinLock` is not interrupt-safe.** Harmless today because interrupts are
   never enabled, but it must gain an interrupt-disabling variant before the
   first handler is registered. Documented at the type.
3. **Bootloader allocations are over-conservative.** Page tables, the handoff
   block and the kernel image are allocated as `RuntimeServicesData`, which the
   kernel must treat as permanently reserved. This wastes on the order of
   100 KiB. A custom UEFI memory type would let the kernel reclaim the page
   tables after it builds its own; deferred until the VMM exists to reclaim them.
4. **The framebuffer is mapped write-back, not write-combining.** Correct in
   QEMU, slow on real hardware. Needs PAT configuration in the kernel.
5. **The kernel binary carries debug info into the ESP** — 4 MiB of which ~32 KiB
   is loadable. Harmless but wasteful of boot time; a stripped image plus a
   separate symbol file is the fix.
6. **VVFAT, not a real disk image.** QEMU synthesises a FAT filesystem from a
   directory. Excellent for iteration, but it means NexusOS has never been
   booted from a genuine partition table. A real GPT + FAT32 image builder is
   needed before any hardware test.
7. **No CI.** Building and testing is manual.

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

1. **GDT, TSS, IDT and exception handlers** — so failures are diagnosable
   instead of silent resets.
2. **Physical frame allocator** — the memory map is already there, nothing
   consumes it.
3. **Virtual memory manager and kernel heap** — required by every subsystem
   above it.
4. **APIC and timer** — the precondition for preemption.
5. **Threads and the scheduler.**

See [NEXUSOS_ROADMAP.md](NEXUSOS_ROADMAP.md) for the full sequence.
