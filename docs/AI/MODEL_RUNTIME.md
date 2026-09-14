# Model runtime

The optional `nexus-ai-core/model` feature now implements real local pretrained
Llama-style float32 inference: checked weight loader, BPE tokenizer, RMSNorm,
RoPE, grouped-query causal attention, per-session KV cache, SwiGLU and greedy
decoding. `libm` provides software-compatible scalar math on the existing
soft-float guest target. The first verified checkpoint is TinyStories 260K.

`ModelProvider` owns model weights and creates private state. `ModelSession`
validates prompt token IDs, context and output budgets, then emits `Prefill`,
`Token` or `Finished` through pull-based `poll()`. Each poll performs at most one
forward pass. Cancellation and provider errors are terminal; repeated completed
polls do not invoke the backend. No context is saved to disk or sent remotely.

`nexus-model` is a separate one-request guest process. It receives only its parent
channel, embeds pinned weights/tokenizer, and allocates a 4 MiB heap. It has no
file/network/spawn grants and is not the permission broker. Generated text is
untrusted; no model proposal is converted into a Tool or capability.

## Worker protocol (one outstanding request per channel)

All frames are exact where indicated; u16/u32 are little-endian. A channel owns
one session; there is no caller-controlled identity or authentication token.
Unexpected transferred capabilities cause worker exit and process cleanup.

| Direction | Frame | Meaning |
|---|---|---|
| Client -> worker | `gen1` + u16 output limit + UTF-8 prompt | Initial request, 6–256 bytes; 1–256 output tokens and prompt + output <= 512 tokens |
| Worker -> client | `rdy1` | Parsed request and initialized session |
| Client -> worker | `next` | Perform one forward pass, or report completion |
| Worker -> client | `pre1` | One prompt token processed; no generated output |
| Worker -> client | `tok1` + u32 token ID + bytes | Generated token, up to 128 piece bytes; pieces may split UTF-8 |
| Worker -> client | `end1` + u8 reason | 0 = EOS, 1 = output length; worker exits |
| Client -> worker | `stop` | Cancel at a pull boundary |
| Worker -> client | `can1` | Cancelled; worker exits |
| Worker -> client | `err1` | Request/inference failure; worker exits unsuccessfully |

Worker waits are bounded to 10 seconds, total generation wall time to 120 seconds
at command boundaries, and commands to 1024. Cancellation/deadline cannot preempt
a currently running forward pass; the pinned model bounds that work. Closing the
peer causes worker exit. A completed session must be replaced with a new worker.

## Verification and use

See [local model instructions](../../tools/nexus-model/README.md),
`shared/nexus-ai/tests/model_checkpoint.rs`, and `user/nexus-ai/src/model_probe.rs`.
Host tests compare two generated sequences against a separately compiled pinned
llama2.c oracle, validate UTF-8 round trips, reject corrupt assets and check
session isolation. The guest test checks actual IPC output against the oracle,
completion, cancellation and invalid limits, followed by a live kernel tick.
Task-specific run evidence is recorded in `.ai_collaboration/tasks/ASTRA-MODEL-001.json`.

This is a small English story model, not a Japanese instruction model. No training,
model routing, remote transport, assistant UI integration or default-boot model
service has been added. The existing read-only `nexus-ai` service remains separate.
