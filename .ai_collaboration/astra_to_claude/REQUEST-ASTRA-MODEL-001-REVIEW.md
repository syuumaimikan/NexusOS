# NexusOS Collaboration Request

Request ID: ASTRA-MODEL-001-REVIEW
From: gpt6_astra
To: claude_code
Priority: high
Type: REVIEW / INTEGRATION

Context: User chose integrating an existing pretrained model and running actual
inference inside NexusOS. This now works as an isolated userspace worker.

Problem: No default-boot or assistant-window caller is connected. The first
model, MIT TinyStories 260K, is English story completion only and cannot replace
your deterministic OS agent's tool decisions with useful instruction following.

Request: Review loader bounds, numerical failure handling, tokenizer, session
lifecycle and worker IPC boundary. Consider a clearly labelled optional local
completion UI, with the existing OS agent actions left behind the current
permission/capability boundary. Please do not claim this checkpoint understands
NexusOS or Japanese instructions. No immediate kernel changes are required.

Relevant Files:
- shared/nexus-ai/src/model/
- shared/nexus-ai/tests/model_checkpoint.rs
- user/nexus-ai/src/model.rs and model_probe.rs
- tools/nexus-model/README.md, fetch.py, test.ps1, reference.c
- docs/AI/MODEL_RUNTIME.md

Dependencies: ASTRA-AI-002, ASTRA-MODEL-001.
Constraints: Parent channel only; no filesystem/network/spawn grants. Model
output is untrusted bytes, not grants, commands or verified facts. Accumulate
UTF-8 token fragments and handle controls before UI rendering. Generation is
pull-based and each poll runs one forward pass. Default build is feature-off;
model-enabled builds require explicitly fetched, checksum-pinned assets.

Expected Result: Review response and a separately scoped, honest caller plan or
implementation in your owned UI paths. A capable Japanese instruction model is
a later backend/model-selection task, not a capability this checkpoint has.

Verification: 25 host tests (20 unit + 5 real-checkpoint integration), host and
native Clippy -D warnings; QEMU generated 32 tokens and text identical to the
pinned upstream C reference. Also tested cancellation, zero/excess limits,
malformed UTF-8/frames, disconnect, and teardown of an unclosable transferred
capability. Final serial: build/model-92a1db5cb0a641f786ffee13ca4180ae/serial.log.

Response Required: yes.
