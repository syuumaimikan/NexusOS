# NexusOS

**A high-performance, AI-native, secure desktop operating system, built from scratch.**

NexusOS is not a Linux distribution and not a Linux fork. The kernel, the
bootloader, the memory manager, the scheduler and everything above them are
written for this project. Linux and Windows application compatibility are
planned as translation layers *above* the Nexus system-call interface, never as
a modified upstream kernel.

## Status

Phases 0 to 5 of 19 are complete: the system boots from UEFI firmware through
its own bootloader into a higher-half kernel, brings up interrupts, physical and
virtual memory, a heap and a preemptive scheduler, and paints a live status
screen.

```
UEFI firmware
  └─ Nexus Bootloader        GOP mode selection, ACPI discovery, ELF loading,
     │                       page tables, ExitBootServices
     └─ Nexus Kernel         higher half at 0xFFFFFFFF80000000
        ├─ interrupts        GDT, TSS with IST stacks, 256-vector IDT, PIT
        ├─ memory            buddy frame allocator, VMM, kernel heap
        ├─ scheduler         preemptive kernel threads, priorities, sleep/wake
        └─ display           bitmap font, live status screen, two languages
```

The interface speaks **English and Japanese**, switchable at runtime. Strings
live in `locales/*.txt`, never in the code, and the CJK glyphs are rasterised at
build time from a font on the build machine — see
[the localisation notes](docs/i18n.md).

What runs today, verified on every boot:

- 1920×1200 BGRX framebuffer, painted by the kernel through the direct map
- 1017 MiB of usable RAM classified across 22 physical memory regions
- Kernel `.text` mapped read-only and executable; everything else non-executable
- A guarded boot stack, so an overflow faults rather than corrupting memory
- Preemptive kernel threads, verified against a thread that never yields
- A live status screen in English or Japanese, redrawn by its own thread

Not yet present: user mode, the local APIC and SMP, drivers, filesystems,
networking, the compositor, or the desktop. "Thread" still means a kernel
thread, and nothing is isolated from anything else yet. See the
[architecture audit](docs/NEXUSOS_ARCHITECTURE_AUDIT.md) for an honest
subsystem-by-subsystem status and the [roadmap](docs/NEXUSOS_ROADMAP.md) for
what comes next.

## Requirements

- Rust nightly with `rust-src` and `llvm-tools` (pinned by `rust-toolchain.toml`)
- QEMU with its bundled edk2 firmware
- PowerShell (the build scripts are PowerShell; the code itself is
  platform-independent)

## Building and running

```powershell
.\scripts\build.ps1          # build both components, stage the ESP tree
.\scripts\run.ps1            # boot in QEMU with a display
.\scripts\run.ps1 -Headless  # boot headless, print the serial log
.\scripts\screenshot.ps1     # boot and capture the guest display as a PNG
```

Serial output lands in `build/serial.log`. It is the primary diagnostic channel
and works before, during and after the handoff to the kernel.

Tests run on the host:

```bash
cargo +nightly test -p nexus-boot --lib
```

## Repository layout

```
shared/nexus-abi/     Layout-stable types shared across the boot boundary
boot/nexus-boot/      The Nexus Bootloader (UEFI application + testable library)
kernel/nexus-mm/      Physical and heap allocators, host-testable
kernel/nexus-kernel/  The Nexus Kernel
locales/              Interface translations, one file per language
targets/              Custom target specification for the kernel
scripts/              Build, run, screenshot and font-generation harness
docs/                 Architecture audit, roadmap, subsystem design notes
```

## Design commitments

**No third-party crates in the boot path.** The UEFI bindings, the ELF loader
and the page-table code are all project code. This is more work and it is what
makes "built from scratch" true rather than decorative.

**Each phase runs before the next begins.** A phase is finished when it boots
and its behaviour is visible in the serial log, not when it compiles.

**Security is designed in, not added later.** W^X on the kernel image and a
guarded stack exist from the first commit; the capability model shapes the
handle table before there are handles to protect.

**The AI subsystem gets capabilities, never ambient authority.** Nexus Agent
will act only through explicit, revocable, audited permissions.

**Japanese is a first-class language, not an afterthought.** It is one of the
two the system ships with, the layout is measured in pixels so full-width text
is not squeezed into a Latin grid, and translations can reorder their arguments
because the build refuses a locale that is missing a key.

## Licence

Apache-2.0.
