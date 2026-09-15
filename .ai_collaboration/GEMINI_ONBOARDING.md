# Gemini 3.1 Pro participation

Reported by the user on 2026-09-15; recorded by gpt6_astra.

Provisional STATE key: `gemini_3_1_pro`. This is a coordination identifier, not
a claim that Gemini has acknowledged it. The user assigned **integration and
verification**. Specific current edits and paths are not yet reported. No
existing lock is transferred. Review the handoff evidence for ASTRA-CONTEXT-001;
its isolated QEMU result is not a claim that concurrent TLS changes are integrated.

Before overlapping development:

1. Read STATE.json, README.md and locks/*.json.
2. Record your agreed identity, task, dependencies and active paths in STATE.
3. Acquire a task-specific lock before editing; do not break another task's lock.
4. Keep other developers' diffs and staged files intact. Commit only your paths.
5. Record actual build/test/QEMU evidence and release your task lock when done.

Current coordination context:

- Astra: ASTRA-CONTEXT-001, context.rs/context_tests.rs/lib.rs and AI probe.
  Prior ASTRA-MODEL-002 optimization edits and ASTRA-DOCS-002 edits remain in the
  shared checkout; their status must be inspected before using those paths.
- Claude: see current STATE and CLAUDE-TLS-010 lock for TLS/browser/kernel work.
- Pending reviews remain in astra_to_claude and claude_to_astra. Do not treat a
  request as accepted or reviewed without a real response.

The nexus-collab CLI accepts an explicit --agent identity for mutations. Its
current request discovery covers the two existing Claude/Astra directories;
three-party request routing has not yet been implemented. Until that is agreed,
use explicit From/To fields in shared collaboration notes and register tasks
and locks in the existing common STATE rather than assuming a new inbox is read.
