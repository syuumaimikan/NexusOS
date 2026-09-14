# Local model bring-up

This runs a pretrained Transformer inside a NexusOS userspace process. It uses
Karpathy's MIT-licensed TinyStories 260K checkpoint (64 dimensions, 5 layers,
8 query heads, 4 KV heads, vocabulary 512, context 512). It is an English story
completion model, not an instruction/chat model or a Japanese assistant. The
model does not know NexusOS APIs and its output never authorizes tool execution.

Weights and tokenizer are downloaded explicitly to ignored `build/models/`.
`fetch.py` pins revisions, verifies exact SHA-256 hashes, and never reads user
files or uploads data. The worker embeds these assets in its read-only binary;
there is no filesystem, network or spawn capability given to the worker.
Inference runs on the guest CPU with software floating point, without a GPU or
changes to the kernel's FPU/context-switching boundary.

## Run

From the repository root, using the existing Windows Rust nightly toolchain:

```powershell
python tools/nexus-model/fetch.py
cargo test --offline -p nexus-ai-core --features model
cargo run --offline -p nexus-ai-core --features model --example infer -- build/models/stories260K.bin build/models/tok512.bin "Once upon a time" 32
powershell -NoProfile -ExecutionPolicy Bypass -File tools/nexus-model/test.ps1
```

If libm 0.2.16 is not already cached, first run `cargo fetch`. The model feature
is opt-in: the normal AI service and build do not require downloaded weights.
The QEMU script uses its own disk, ESP and serial log under `build/model-<GUID>`.
It stages the worker as `BIN/MODEL.ELF` and the integration client as `INIT.ELF`.
No normal desktop installation or assistant-window integration is implied.

To reproduce the independent reference on a host with GCC:

```sh
gcc -O2 -I build/models -o build/models/oracle tools/nexus-model/reference.c -lm
build/models/oracle build/models/stories260K.bin build/models/tok512.bin 'Once upon a time'
build/models/oracle build/models/stories260K.bin build/models/tok512.bin 'The little cat'
```

The reference compiles the downloaded, checksum-verified upstream C implementation.
Its 32-token results are checked into the Rust integration test and guest probe.
Neither reference results nor model weights are synthesized by the new backend.

## Limits

Greedy decoding only; no sampling, chat template, remote provider, embeddings,
vision, tool calling, or training. Model loader supports a bounded subset of the
legacy llama2.c float32 layout (16 MiB checkpoint, 256 dimensions, 8 layers,
4096 vocabulary entries, 512 context positions). This is not a GGUF loader and
cannot load arbitrary contemporary LLMs. Allocation failure terminates a guest
worker; the kernel does not yet impose a general model-worker resource quota.

UTF-8/Japanese input is encoded with byte fallback and round-trip tested. This
preserves text bytes; it does not make the English-trained model competent in
Japanese. Token fragments are bytes, which clients must accumulate before UTF-8
rendering. Treat controls and generated text as untrusted in a future UI.

The next deployment step is a separately reviewed assistant-window caller and
selection of a useful Japanese instruction model plus an appropriately sized
backend. This tiny checkpoint proves local inference, not assistant quality.

## Sources and license

- Model: https://huggingface.co/karpathy/tinyllamas/tree/0bd21da7698eaf29a0d7de3992de8a46ef624add/stories260K
- Model card (MIT): https://huggingface.co/karpathy/tinyllamas/blob/0bd21da7698eaf29a0d7de3992de8a46ef624add/README.md
- Reference architecture and format: https://github.com/karpathy/llama2.c/tree/350e04fe35433e6d2941dce5a1f53308f87058eb
- Upstream copyright and license: [LICENSE.llama2.c](LICENSE.llama2.c).

The Rust mathematical implementation follows the upstream architecture and is
accompanied by its MIT notice. Downloaded pretrained assets remain in build/;
retain the upstream notice if distributing them with a worker binary.
