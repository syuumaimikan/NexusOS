# Policy and service availability

`requirement(tool)` describes the permission category and safety level.
`permitted(tool)` evaluates that classification independently of which tools a
particular executor implements. `Ok(())` is not a capability or user approval:
the executor must possess a suitably scoped OS handle and validate arguments.
File contents remain untrusted and must not be sent to a remote model merely
because reading them is classified Safe.

`authorize(tool)` additionally checks the fixed AI service allowlist. The
Runtime calls this API before invoking its backend. Only SystemInfo is admitted;
FileRead is classified safe but remains Unsupported in this service. Writes,
network and process control still require confirmation, configuration remains
privileged, and kernel memory is blocked. No approval/grant API was added.

An executor such as nexus-assist, which already holds its own read-only file
capability, can use `permitted` for classification without claiming that the AI
IPC service implements file reading. Migration of that caller belongs to Claude.

Sessions correlate work and are not authentication tokens. Valid denied requests
consume sequencing IDs and the 64-request budget. Zero IDs in error responses
mean an unattributable malformed frame. The current uptime/thread observation
pair checks monotonic time and stable service-thread identity; it does not prove
machine-wide health.

ASTRA-AI-002 adds regression coverage for every tool's policy, service outcome
and backend call count, plus zero-ID error response handling. No model provider,
context engine or full MVP is claimed by this change.

Verification: 15 host tests passed; host and native-target Clippy passed with
`-D warnings`. Isolated QEMU test passed (IPC, policy, observation, teardown
and subsequent kernel monitor tick). Serial evidence: `build/ai-3ad2ff38cea44b04978fc7ba8eadf316/serial.log`.
The run built the current shared working tree, including concurrent non-AI edits.
