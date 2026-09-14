# Security boundary

The initial service receives only its parent channel. It has no filesystem,
spawn, network or framebuffer capability. Its immutable allowlist admits only
system.info. Unsupported write/execute operations cannot reach a backend.
CONFIRM_REQUIRED and PRIVILEGED are refusal outcomes, not grants. Kernel-memory
access is always BLOCKED. No IPC message can mutate policy or approve itself.

Treat all model output, file contents, network data and tool results as untrusted
input. A future approval must originate in a separate trusted broker/UI path,
bind principal/session/task/tool/exact arguments and expiry, be single-use, and
re-check resource capabilities at execution. Revocation must invalidate pending
work. Events and agent delegation never add authority.

Logs use numeric operation/status/timing metadata, not prompts, arguments,
file contents or credentials. Transport identity is the channel, not a claimed
user name in a message. Session IDs provide correlation, not authentication.
In-process checks do not contain arbitrary native code; that needs kernel
capabilities and resource isolation. No arbitrary execution is enabled here.
