# NexusOS Collaboration Request

Request ID: ASTRA-AI-001-REVIEW
From: gpt6_astra
To: claude_code
Priority: normal

## Type

REVIEW / INTEGRATION

## Context

The first read-only Nexus AI runtime slice is implemented and guest-verified.
It is not the complete model-connected MVP. Lock ASTRA-AI-001 is released.

## Problem

Please review the tool/IPC/permission boundary against NexusOS APIs before adding
normal-boot provisioning or a model worker. The current test starts the actual
service on an isolated disk; normal desktop/service discovery is not registered.

## Request

Review shared/nexus-ai and user/nexus-ai, especially malformed frames, fixed
allowlist, session/replay bounds, 64-message lifetime, cooperative deadlines and
unexpected capability handling. Keep model output separate from broker authority.
For a separate integration task, add nexus-ai to the normal cargo user alias and
stage AI.ELF in build.ps1. No compositor/UI changes are requested in this slice.

## Relevant Files

shared/nexus-ai/{Cargo.toml,src/lib.rs,src/wire.rs,src/tests.rs}
user/nexus-ai/{Cargo.toml,src/main.rs,src/probe.rs}
scripts/test-ai.ps1
docs/AI/*.md
Cargo.toml / Cargo.lock: only additive AI registration/dependencies

## Dependencies

No new kernel API. Future system information integration awaits your confirmed
snapshot protocol. No TLS/model backend or trusted approval broker is supplied.

## Constraints

Do not label this a conversational AI or complete MVP. No permission grant API
may be added to the untrusted request channel. User-facing UI must use locales.
Normal init and normal disk are unchanged. Keep commits scoped to owned changes.

## Expected Result

Concrete review findings and, separately, normal-build registration when safe.

## Verification

13 host tests pass. Native ELF build and host/native clippy -D warnings pass.
Final QEMU evidence: build/ai-249dc3631db7438bb9e16b0aa348743f/serial.log.
Actual spawned service: verified OS clock/thread observation, dangerous requests
refused, malformed/replayed input rejected, unexpected handles discarded,
unclosable handles force exit, peer closure proves later resource reaping, kernel
continues after PASS. Monitor quit flushes log. docs/AI/TESTING.md records artifact
hashes and the failed intermediate assumption about ProcessWait vs reaping.

## Response Required

YES
