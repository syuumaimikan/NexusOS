# Delivery roadmap

Phase 1 audit precedes implementation. First slice covers bounded runtime core,
IPC and read-only policy/verification. Remaining phases are not completed merely
because they are described here.

| Order | Deliverable and acceptance gate |
|---|---|
| 2–6 | Runtime, IPC, model interface, tool registry, permissions: validated wire input and no execution on denial; initially read-only system.info. Model adapters require a real usable backend. |
| 7–8 | Context and memory: bounded selection, source/sensitivity metadata, retention expiry and explicit persistence opt-in. Reuse nexus-index where suitable. |
| 9–11 | Agents, durable DAG planning and verification: recover task state, no cycles, bounded correction, independent filesystem read-back tests. |
| 12 | Sandbox: broker-issued directory handles, no ambient spawn/network, OS-enforced resource limits before arbitrary code execution. |
| 13–14 | Service discovery/supervision and GUI: trusted approval identity, cancellation and multi-session stream routing; actual guest UI request-to-result tests. |
| 15–16 | Coding and delegation: real guest compiler/toolchain, scoped workspace, builds/tests and attenuation across agents. |
| 17–18 | Local inference (model/tokenizer/quantization/KV/batching) and remote adapters (authenticated TLS, credential store, egress consent). |
| 19–20 | Full QEMU UI E2E, fault/stress tests, performance baselines, adversarial permission tests. |

Full completion requires the user-visible natural-language → plan → authorized
tool → verification → response flow in NexusOS. Protocol smoke tests alone do
not meet that definition.
