# Contributing

Read .ai_collaboration/STATE.json, pending requests and locks before editing.
Reuse nexus-user and existing crate layout. Lock AI paths; coordinate kernel,
compositor, locales and build script changes with Claude Code. Do not include
other developers' staged changes in a commit.

Keep shared/nexus-ai no_std and independent of kernel internals. New executable
tools require a real adapter, immutable schema, permission gate, verified result,
bounds and rejection tests proving the backend was not invoked. Never add a
fake model or success stub to make an E2E test green. Test doubles are restricted
to host failure-injection tests and never linked into the guest service.

Use cargo fmt -p nexus-ai-core -p nexus-ai, host tests, native-target build/clippy,
and scripts/test-ai.ps1. Record evidence and limitations in TESTING.md. User UI
strings belong in both locales; current serial traces are developer diagnostics.
Update task/state/events and release locks once work is verified. Send Claude a
concrete result/review request with changed files, API impact and actual checks.
