# NexusOS integration audit

Date: 2026-09-14. Source inventory and integration audit, not a claim that every
implementation has been security reviewed. Initial HEAD: a50612e. Existing local
changes: `scripts/test-input.ps1`, `user/nexus-shell/src/main.rs`; leave them intact.
No AGENTS.md was found in the repository. The suggested shared documents
`docs/CURRENT_STATE.md`, `docs/ARCHITECTURE.md`, `docs/TECHNICAL_DEBT.md` are absent.
README and the September 8 architecture audit substantially lag current code.

| Area | Current source evidence and integration implications |
|---|---|
| Build/crates | Root Cargo workspace, local Rust crates under boot/kernel/shared/user/tools. Nightly is floating, not pinned. `.cargo/config.toml` defines separate boot/kernel/user builds. PowerShell scripts assume Windows executables. |
| Boot/CPU/ABI | `boot/nexus-boot` loads ELF64 after UEFI GOP/ACPI discovery and exits boot services. x86_64, static binaries, SysV entry, custom kernel and user targets. User target disables SSE/AVX and uses soft float: native inference backends need platform work. |
| Kernel/memory | `kernel/nexus-kernel/src/memory`, `shared/nexus-abi/src/layout.rs`: higher half kernel, direct physical map, per-process lower half tables, W^X and guarded stacks. `nexus-mm` buddy/free-list allocators are host testable. |
| Scheduler/processes | `sched/`, `process.rs`, `user.rs`: SMP/APIC preemption, priorities, sleep/wake, process completion/kill, own address spaces and handle tables. Spawn is a service channel; args are an initial channel message. No general POSIX process environment. |
| Syscalls | `arch/syscall.rs` and `nexus-user`: 32 calls (0–31), syscall/sysret, range/rights checks, handles, node operations, waitsets, uptime/RTC. Numbers are duplicated across kernel/user; AI must use wrappers. No exported global memory-statistics or process-list call. |
| IPC/services | `ipc.rs`: 256-byte messages, 4 transferred handles, 64 queued messages; moved handles, per-handle READ/WRITE/CLOSE/TRANSFER. Waitsets support deadlines. Spawn/network/sound services are kernel threads behind capability channels, not a generic userspace service registry. |
| Filesystems | `fs/`: GPT, FAT32 program loading, writable NexusFS with cache/store. `nexus-user` provides directory-relative single-component node open/create/read/write/list/remove and read/write-at. Directory capability scopes access; do not invent global path authority. |
| Drivers | PCI, legacy virtio block/network, RTC, PS/2 keyboard/mouse, speaker. No inference GPU/NPU driver or general accelerated compute API identified. |
| Graphics/GUI | GOP framebuffer; userspace compositor owns surfaces, focus/input, damage/resize. `nexus-ui` drawing and `nexus-window::App`/Window event loop exist. Desktop shell, terminal, settings, setup, browser, wallpaper, store exist. AI panel should be another client. |
| Networking | virtio-net + IPv4/ARP/ICMP/UDP/TCP; channel-based network service, shared netclient/DNS/HTTP parsers. `docs/browsing.md` explicitly states HTTPS unavailable. Remote secrets must not cross plaintext HTTP. |
| Security | Capability narrowing/transfer, process isolation, signed packages and password hashing exist. No complete per-process CPU/memory/network quota sandbox, secret vault, or trusted AI approval broker identified. |
| Applications/config | Built-in terminal commands and spawn service, limited static Linux compatibility; no guest Rust compiler/build environment identified. `nexus-config` parses UTF-8 key=value; settings app writes system/settings.txt. |
| Existing AI-like logic | `nexus-find` searches a lent directory with `nexus-index` hashed character n-grams. This is deterministic retrieval, not an LLM provider or general agent runtime. Reuse later as a retrieval source. |
| Tests/QEMU | Host library tests; PowerShell formatting/clippy, boot-marker, persistence, input, image, network, GUI, soak and fault tests. q35/4 CPUs/1 GiB, edk2, serial output. Existing staged artifacts are not evidence of a fresh build. |
| Languages | Rust source and inline/naked assembly; PowerShell build/test/font generation. No separate C/C++ implementation found in source inventory. |

## First implementation unit

An independent allocation-free `shared/nexus-ai` runtime and userspace service,
using only uptime/thread-id syscalls. A bounded versioned IPC protocol, immutable
read-only allowlist, execution/observation/verification states, replay rejection,
and actual guest process/IPC tests precede mutation tools or a model connection.
This does not implement natural-language inference and will not impersonate it.
Use an isolated QEMU disk and test-init program to avoid changing the desktop or
normal init while Claude Code is editing nearby areas.

## Environment observations

Linux cargo/rustup/pwsh/QEMU are not on PATH. Windows cargo and QEMU are installed.
WSL sandbox initially rejects Windows interoperability (vsock); cargo version
works with execution escalation. Verification results are recorded in TESTING.md.

## Collaboration update after the initial audit

Claude subsequently added `.ai_collaboration/NEXUSOS_STATE.md`, README, requests,
and self-reported state. His Window/settings/store fixes landed at 15f7b56.
Astra reviewed the design/source and sent responses; other changes are not
included in ASTRA-AI-001. System-information service ownership is confirmed with
Claude; payload design feedback is in RESPONSE_SYSTEM_INFO_DESIGN.md.
The first read-only slice is now guest-verified; see TESTING.md for exact limits.
