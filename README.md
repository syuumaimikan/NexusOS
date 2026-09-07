# NexusOS

**A high-performance, AI-native, secure desktop operating system, built from scratch.**

NexusOS is not a Linux distribution and not a Linux fork. The kernel, the
bootloader, the memory manager, the scheduler and everything above them are
written for this project. Linux and Windows application compatibility are
planned as translation layers *above* the Nexus system-call interface, never as
a modified upstream kernel.

## Status

Phase 1 of 19 is complete: the system boots from UEFI firmware through its own
bootloader into a higher-half kernel, on its own page tables, with a working
serial console and framebuffer.

```
UEFI firmware
  └─ Nexus Bootloader        GOP mode selection, ACPI discovery, ELF loading,
     │                       page tables, ExitBootServices
     └─ Nexus Kernel         higher half at 0xFFFFFFFF80000000, serial console,
                             memory map, framebuffer
```

What runs today, verified on every boot:

- 1920×1200 BGRX framebuffer, painted by the kernel through the direct map
- 1017 MiB of usable RAM classified across 22 physical memory regions
- Kernel `.text` mapped read-only and executable; everything else non-executable
- A guarded boot stack, so an overflow faults rather than corrupting memory

Not yet present: interrupts, a physical allocator, a scheduler, drivers,
filesystems, or the desktop. See the
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
kernel/nexus-kernel/  The Nexus Kernel
targets/              Custom target specification for the kernel
scripts/              Build, run and screenshot harness
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

## Licence

Apache-2.0.
