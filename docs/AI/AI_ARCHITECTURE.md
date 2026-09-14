# Nexus AI architecture

The first slice is a no_std allocation-free library plus an isolated userspace
service. Only the service calls nexus-user; the core has no kernel dependency.
The caller holds the service channel, which scopes a single session. Requests
contain no grants, approval bits, arbitrary handles, executable text or secrets.

Target layering: UI → agent/model worker → trusted permission/tool broker →
existing capability-based OS services → kernel. A model worker must never share
an address space or mutable policy with the broker. A Rust policy object alone
is not a security boundary against compromised executable code.

Future components: agent manager, model provider, context ranking/filtering,
retention-controlled memory, bounded task DAG, execution and verification engine,
event queue and typed delegation protocol. Each adapter must advertise actual
capabilities; missing operations return unavailable, never generated success.
UI consumes observed task events and uses an independent trusted approval route.

The initial runtime exposes one verified tool: system.info (uptime and the
service thread ID only). No global memory/process statistics are inferred.
The target multi-step planning loop is not yet implemented by this first slice.
