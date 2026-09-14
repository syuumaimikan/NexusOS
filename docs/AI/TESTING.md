# Verification results

Date: 2026-09-14. Scope: ASTRA-AI-001 read-only runtime and actual guest IPC
service, not complete Nexus AI MVP or conversational/UI E2E.

## Passed

* `cargo test --offline -p nexus-ai-core`: 13 tests, 0 failures.
* `cargo fmt -p nexus-ai-core -p nexus-ai`: formatted; check command below.
* Host core and native user-target clippy with `-D warnings`: passed.
* Bootloader, kernel, existing userspace and new AI binaries built successfully
  with the existing custom-target/build-std machinery in test-ai.ps1.
* `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-ai.ps1`:
  final run passed on QEMU q35, 4 CPUs, 1 GiB, edk2. Real service spawned via
  existing kernel spawn channel, replies decoded from actual guest IPC.

Final serial evidence:
`build/ai-249dc3631db7438bb9e16b0aa348743f/serial.log`.

Observed: service ready; system.info Verified using actual uptime/thread-id;
TerminalExecute ConfirmRequired, KernelMemory Blocked, SettingsWrite Privileged,
FileRead Unsupported; duplicate ID and malformed frame Invalid. A transferred
closable channel is discarded and its peer closes. An unclosable transferred
channel forces service exit; the probe separately waits for asynchronous reaping
and observes its peer close. After PASS, the kernel emits another monitor report.
QEMU shuts down through monitor quit. Normal disk/ESP/serial were not overwritten.

## Regressions found while implementing

Initial probe used a nonexistent Ending.status field. Compilation rejected it;
fixed to match the existing Ending::Exited enum before guest tests.
Initial hardening probe assumed process completion implied immediate capability
release. A real QEMU run failed that assumption. The process publishes completion
before scheduler reaping. Fixed the probe to wait on peer closure with its own
10-second deadline and assert Error::Closed; final run passed. Failed evidence
retained at `build/ai-b83a04dc1e7a44d98887fe3ed230aaa6/serial.log`.

During a parallel selective commit the root manifest temporarily lost AI member
lines, so clippy reported package IDs missing. Inspected the new HEAD, restored
only the two AI member lines while preserving nexus-json, then both clippy runs
passed. No kernel or other developer changes were reverted.

## Reproduce

Windows development environment, Rust nightly/rust-src, QEMU on PATH:

```powershell
cargo fmt -p nexus-ai-core -p nexus-ai -- --check
cargo test --offline -p nexus-ai-core
cargo clippy --offline -p nexus-ai-core --all-targets -- -D warnings
cargo clippy --offline -p nexus-ai --target targets/x86_64-nexus-user.json -Zbuild-std=core,compiler_builtins,alloc -Zbuild-std-features=compiler-builtins-mem -- -D warnings
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-ai.ps1
```

The script creates a unique build/ai-<GUID> directory each run and uses existing
stage.ps1, make-disk.ps1 and qemu.ps1. Its test INIT.ELF is never installed on the
normal disk. Exit code is nonzero on test/build/guest failure or timeout.

## Not verified / not implemented

No real model provider, natural-language planner, streaming sessions, persistent
memory, GUI assistant, default-boot AI service registration, arbitrary execution
sandbox, coding agent or full MVP. No performance benchmark or arbitrary native
code security guarantee. Host model/context/memory/UI tests are not claimed.
Broader settings/store/terminal QEMU results reported by Claude are independent;
this task does not relabel those reports as its own tests.

## Artifact identity

* `build/ai-249dc3631db7438bb9e16b0aa348743f/serial.log` SHA-256: `f48be249dd2710c8f1d7b5ebc66002a064d3bb59f14f679dbd9fadb7f380e25d`
* `build/ai-249dc3631db7438bb9e16b0aa348743f/programs/ai.elf` SHA-256: `9807b86778486eabc0f570740c2f2801db003ef61cb9b832145e15255f4c4f4f`
* `build/ai-249dc3631db7438bb9e16b0aa348743f/programs/init.elf` SHA-256: `ee2d9f3126ba1a316c6b8ee5e8e666275ca438c40998db3e9b304139958c8c8d`
* `build/ai-249dc3631db7438bb9e16b0aa348743f/esp/nexus/kernel.elf` SHA-256: `10fbf2d36677937126660d2d68e27cd5ab2b9dde77b5b923a66b469d353f6c9f`
