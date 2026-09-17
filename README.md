# NexusOS

**An independent Rust operating system, with a capability-based desktop and an
emerging AI operating layer.**

NexusOS is built around its own kernel, UEFI bootloader, memory manager,
scheduler, filesystem and userspace. It is not a Linux distribution or fork.
The goal is an OS that people and AI agents can operate together through
explicit, scoped authority.

**Status — 2026-09-15:** NexusOS has ring-3 processes, SMP scheduling, bounded
IPC, persistent storage, networking and a graphical desktop. Nexus AI now has a
real, read-only userspace service, bounded context selection and an opt-in local
TinyStories inference worker verified in QEMU. A separate keyword-based assistant
window is present; connecting it to the model and general autonomous computer
operation remain under development.

[Build and run](#build-and-run) · [Current capabilities](#current-capabilities) ·
[Nexus AI](docs/AI/README.md) · [Documentation](#documentation) ·
[Contributing](#contributing)

日本語入力・表示に対応しています。Nexus AIの実装範囲・検証手順は
[AI開発ガイド](docs/AI/README.md)を参照してください。

## Current capabilities

This is an implementation inventory. Individual verification results belong to
the relevant test reports; the presence of a subsystem does not imply complete
hardware support or production readiness.

| Area | Present in the repository | Important boundary |
|---|---|---|
| Boot | UEFI loader, GOP display setup, ELF64 loading, ACPI discovery | x86_64 target; no general hardware certification |
| Kernel | Interrupts, local/I/O APIC, SMP, preemptive scheduling, sleep/wake | Custom kernel and ABI |
| Memory | Buddy allocator, kernel heap, per-process address spaces, guarded stacks, W^X mappings | User target uses soft float; local inference is a bounded scalar backend |
| Processes and IPC | Ring 3, spawn service, completion/kill, narrowed handles, shared memory, waitsets, byte-stream pipes | No `fork`; a Nexus program starts with one thread |
| Storage | GPT, FAT32 program loading, writable NexusFS, block cache | See [NexusFS](docs/nexusfs.md) for format and durability limits |
| Drivers | PCI, legacy virtio block/network, PS/2 keyboard/mouse, RTC, speaker | Limited device families; GPU/NPU inference is unavailable |
| Desktop | Userspace compositor, surfaces, focus/input, resize, window client library | Software framebuffer rendering |
| Applications | Desktop shell, terminal, browser, files/search, settings, setup, wallpaper, package store | Native applications use explicit service/directory handles |
| Network | IPv4, ARP, ICMP, UDP/TCP, DNS, HTTP and browser HTTPS | [TLS 1.3 client](shared/nexus-tls/src/lib.rs) has a limited cipher/group set; no TLS 1.2 |
| Packages | Signed package tooling, installation and update logic | Development keys are for testing; see [keys](keys/README.md) |
| Language | English/Japanese strings, UTF-8 rendering, kana input support | See [Japanese input](docs/japanese-input.md) for scope |
| Nexus AI | Bounded tool runtime, permission gates, observed-result verification, transient context, guest service | Service executes only `system.info`; separate heuristic assistant and opt-in tiny English model are not connected |
| Linux compatibility | Translated x86-64 calls above the Nexus interface: files, memory, threads and futexes, signals, pipes, `poll`/`epoll`, `AF_UNIX` sockets with `SCM_RIGHTS`, `execve`, `ET_DYN` and `PT_INTERP` loading, shared memory by descriptor, a window through the compositor, a Wayland window negotiated through `xdg-shell` and then drawn, resized, typed at and closed, and static i386 programs in compatibility mode | No libc is present; no `fork`, kernel-raised signals or `AF_INET`; 32-bit is limited to calls that pass no structures; the Wayland side has never been run against `libwayland` itself, and there is no X11, no Vulkan and no OpenGL. **Steam does not run** — see [Steam and graphics](docs/steam-graphics.md) |

## Architecture

```text
UEFI firmware
  └─ nexus-boot
      └─ Nexus kernel
          ├─ memory, scheduler, interrupts, processes
          ├─ capability handles, channels, waitsets
          ├─ filesystem, device drivers, network services
          └─ userspace
              ├─ compositor → surfaces → native applications
              ├─ nexus-user / nexus-ui / nexus-window libraries
              └─ nexus-ai service → existing OS APIs
```

A program's filesystem, process-control and network access comes from handles
it is explicitly given. IPC can transfer a handle with reduced rights; it cannot
manufacture greater authority. The AI service uses the same boundary as other
applications and does not access kernel internals.

The current service architecture uses bounded request/reply channels. Window
clients use waitsets to keep input and service replies responsive. Long-running
AI/model work belongs in separate workers, with a trusted tool broker governing
operations. See [AI architecture](docs/AI/AI_ARCHITECTURE.md) and
[security](docs/AI/AI_SECURITY.md).

## Build and run

### Development environment

The maintained build/test scripts target **Windows PowerShell** and Windows
Rust tools. Required:

- Rust nightly with `rust-src`, `llvm-tools`, `rustfmt` and `clippy`.
- The `x86_64-unknown-uefi` target; custom kernel/user targets are in `targets/`.
- QEMU with bundled edk2 firmware, and `qemu-system-x86_64` on PATH.
- PowerShell and a suitable Japanese font for generated CJK glyphs.

`rust-toolchain.toml` selects nightly and its components. It currently follows
nightly rather than pinning a dated toolchain. The scripts contain Windows paths
and executable assumptions; a native Linux build harness is not supplied.

### Start the desktop

Run from the repository root:

```powershell
.\scripts\build.ps1
.\scripts\run.ps1
```

On an unconfigured disk the setup application collects initial preferences.
See [first run](docs/first-run.md), [terminal](docs/terminal.md) and
[settings](docs/settings.md).

```powershell
# Release binaries and an interactive boot
.\scripts\build.ps1 -Release
.\scripts\run.ps1 -Release

# Headless run and display capture
.\scripts\run.ps1 -Headless -Timeout 60
.\scripts\screenshot.ps1
```

The primary diagnostic output is `build/serial.log`. Boot/runtime markers are
more useful than assuming a fixed number of seconds is sufficient on every host.
`scripts/run.ps1 -Until <marker>` can wait for a specific marker and a subsequent
monitor report, bounded by `-Timeout`.

### Build outputs and test data

| Path | Purpose |
|---|---|
| `target/x86_64-unknown-uefi/` | Bootloader build outputs |
| `target/x86_64-nexus/` | Kernel ELF and symbols |
| `target/x86_64-nexus-user/` | Native userspace ELF binaries |
| `build/esp/` | Firmware-facing boot files |
| `build/programs/` | Staged programs and test packages |
| `build/nexus-disk.img` | Generated guest disk; may contain guest data |
| `build/serial.log` | Normal run diagnostics |
| `build/ai-<GUID>/` | Isolated AI test disk, staged binaries and serial evidence |

Treat generated disks used by test scripts as disposable test data. Normal build
staging can rebuild a stale disk. `build.ps1 -DeepSelfTest` enables destructive
filesystem exercises for test runs; it is not the normal desktop build mode.
The AI test creates its own unique disk and does not replace the normal disk.

## Testing

Use focused checks during development:

```powershell
cargo test --offline -p nexus-boot --lib
cargo test --offline -p nexus-mm --lib
cargo test --offline -p nexus-ai-core

# Build aliases select the proper target and build-std flags
cargo bootloader
cargo kernel
cargo user
```

The full OS suite covers formatting/lints, host tests, boot markers, filesystem
persistence, input, application behavior and fault injection:

```powershell
.\scripts\test.ps1
.\scripts\test-settings.ps1
.\scripts\test-store.ps1
.\scripts\test-terminal.ps1
.\scripts\soak.ps1 -Count 10
```

Use dedicated guest test disks when running filesystem/fault tests. A successful
compile is not proof that a driver, IPC interaction or GUI path works in the
running OS.

### Verify Nexus AI

```powershell
.\scripts\test-ai.ps1
```

This builds the OS and AI binaries, stages an isolated disk, starts the real AI
service through the existing spawn channel, checks permissions and observed
results, verifies resource cleanup, and requires the kernel to continue running.
It substitutes the test disk's `INIT.ELF` with the probe. The normal build stages
`AI.ELF` and the separate heuristic `ASSIST.ELF`; model assets remain opt-in.

The initial runtime evidence is in [TESTING.md](docs/AI/TESTING.md). The later
[Context task report](.ai_collaboration/tasks/ASTRA-CONTEXT-001.json) records
**38 host tests with the model feature**, clippy and isolated QEMU verification;
shared-tree integration and review are tracked separately. For actual pretrained
inference, follow the opt-in [local model instructions](tools/nexus-model/README.md).
These component results do not establish model-driven assistant GUI E2E.

## Repository layout

```text
boot/nexus-boot/         UEFI bootloader and host-testable loader logic
kernel/nexus-kernel/     Kernel, drivers, processes, IPC and system services
kernel/nexus-mm/         Reusable memory allocators
shared/                 ABI, formats, parsers, localization and other libraries
shared/nexus-ai/         Allocation-free AI core, policy, verification and wire format
user/                   Native applications and userspace libraries
user/nexus-ai/           Read-only AI service, model worker and guest probes
user/nexus-assist/       Keyword-based assistant window
user/nexus-window/       Compositor client and application event loop
user/nexus-ui/           Canvas/text/layout drawing
locales/                English and Japanese translation sources
tools/                  Host-side package and compatibility tooling
targets/                Custom x86_64 kernel and userspace target specifications
scripts/                Build, staging, QEMU and test harnesses
docs/                   Subsystem documentation and roadmap
docs/AI/                AI audit, implementation guide, security and test evidence
.ai_collaboration/      Shared agent state, tasks, requests, decisions and events
```

## Documentation

| Start here | Contents |
|---|---|
| [Current source audit](docs/AI/CURRENT_STATE.md) | OS capabilities and AI integration points observed in source |
| [Nexus AI guide](docs/AI/README.md) | Implemented scope, protocol, checks and next milestones |
| [OS roadmap](docs/NEXUSOS_ROADMAP.md) | Longer-term subsystem development and limitations |
| [Architecture audit — historical](docs/NEXUSOS_ARCHITECTURE_AUDIT.md) | September 8 baseline; several “missing” entries are now implemented |
| [Collaboration briefing](.ai_collaboration/NEXUSOS_STATE.md) | API and architectural constraints shared across collaborators |
| [Storage](docs/nexusfs.md) / [packages](docs/packages.md) / [updates](docs/updates.md) | Persistent data and installation |
| [Networking](docs/networking.md) / [browser](docs/browsing.md) | Network APIs and browser behavior; browser guide predates TLS support |
| [Appearance](docs/appearance.md) / [sound](docs/sound.md) | Native desktop facilities |
| [Localization](docs/i18n.md) / [Japanese input](docs/japanese-input.md) | Strings, fonts and input |
| [Linux software](docs/linux-software.md) / [Steam and graphics](docs/steam-graphics.md) | What a program built for Linux can do here, and what it cannot |

## Contributing

Read [the collaboration protocol](.ai_collaboration/README.md), then current
state, requests and locks under `.ai_collaboration/`. Coordinate shared-file
changes before modifying another developer's active paths. Keep commits scoped
to your own changes.

Reuse existing crates and APIs. Make bounded, reviewable changes; test the
failure path as well as success. `unsafe` requires a `SAFETY` explanation.
User-facing text belongs in both locale files. Missing features must be reported
as missing, never hidden behind dummy success responses.

For AI changes, follow [AI contributing](docs/AI/CONTRIBUTING.md). A new tool
needs a real adapter, a permission gate, execution bounds and verification.
The model must never be able to authorize its own dangerous operation.

## Licence

Apache-2.0.
