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
- SMP bring-up via the MADT, per-CPU data
- Ring 3 transition, `syscall`/`sysret`

## Phase 6 — Handles, IPC, system calls ⬜

- Handle table with per-handle rights, the root of the capability model
- Channels, shared memory, events, semaphores, mutexes
- The Nexus system-call ABI
- Zero-copy message passing

## Phase 7 — Storage and NexusFS ⬜

- virtio-blk, then NVMe and AHCI
- Block cache, VFS layer
- NexusFS: copy-on-write, journaling, checksums, snapshots, compression

## Phase 8 — Drivers and user space ⬜

- PCI/PCIe enumeration, MSI/MSI-X, IOMMU
- User-space driver model over IPC
- `init`, service manager, libc, runtime, shell

## Phase 9 — Graphics and the compositor ⬜

- Nexus Graphics abstraction over the framebuffer, later a GPU
- Nexus Compositor: surfaces, damage tracking, frame scheduling, multi-monitor
- Input: keyboard, mouse, touchpad

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
