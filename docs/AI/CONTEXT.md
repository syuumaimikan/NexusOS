# Context selection

`nexus-ai-core::context` implements bounded, transient context selection over
authorized inputs, with a verified Runtime observation adapter and support for
the optional model's actual tokenizer. It performs no I/O or capability grants.

## Data flow and API

```text
Authorized collector -> scope/time/privacy checks -> whole-entry redaction
 -> priority/relevance/recency -> byte/token budget -> text + separate provenance
 -> Tokenizer::encode -> ModelSession::new -> poll
```

`Runtime::context()` returns a `SystemContext` only after the existing two-read
verifier succeeds. Every new request clears it before validation; denied,
replayed, failed and cancelled requests cannot leave previous facts current.
Copies already held by callers are historical; selection still checks freshness.
`SystemContext::write()` formats observed uptime and service thread ID only.
The richer `shared/nexus-machine` service exists but is not consumed by this
adapter. Neither CPU/RAM values nor machine-wide health are inferred.

`Context::select(entries, policy, counter)` accepts at most 16 entries of at most
1,024 UTF-8 bytes each, and stores at most 1,024 bytes in total, without heap
allocation. The trusted collector supplies each entry's origin, exact session
and task scope, collection time, relevance, priority and sensitivity. Runtime
uses request ID as task ID. There is no implicit global scope.

Unauthorized, irrelevant, empty, oversized, stale, future-dated or wrong-scope
inputs are excluded before tokenization. Private content requires allow_private.
Remote destinations additionally require explicit remote_allowed on each entry,
including public ones. Neither flag grants OS authority.

Higher priority, then relevance, then recency wins. Ties retain input order.
Items that do not fit are skipped so smaller items can fit. Whole entries are
separated by newlines, never truncated within UTF-8. Selected metadata retains
input index, source, timestamp and exact byte span separately from content.

The tokenizer counts the complete candidate text, including separators and its
special tokens. The optional `model::Tokenizer` implements `TokenCounter` with
actual BPE encoding. Counter failure aborts selection rather than returning a
partial success. `ByteBpeBudget` supplies a conservative bytes+2 bound for the
current byte-fallback BPE only; it is not universal for other tokenizers.
Zero reported tokens means no context is injected, not the encoding of empty text.

The caller must reserve instructions and output separately, tokenize the final
combined prompt, and use ModelSession's context-limit checks. Selection's token
budget covers the selected context alone.

## Privacy and security

Metadata comes from a trusted collector **after** OS capability checks, never
from model output or a document's self-description. Selected text is untrusted
data; embedded instructions cannot acquire tool permissions through selection.

Secret-classified entries are always excluded. Defense-in-depth marker checking
also excludes entire entries containing common password/API-key/auth-header,
private-key and token-prefix markers, including Japanese password/private-key
labels. No credential prefix or partial value survives in selected storage.
Matching is deliberately conservative and can exclude benign text. Unknown,
encoded or obfuscated credentials are not universally detectable: correct
classification and minimal authorized collection remain the primary boundary.

No text is logged or persisted. Reports contain counts only; content-bearing
types omit Debug. `clear()` and Drop overwrite selected storage and metadata.
This does not erase caller-owned inputs or compiler/stack copies and is not a
cryptographic memory-erasure guarantee. There is no conversation history here.

## Verification

```powershell
cargo test --offline -p nexus-ai-core
cargo test --offline -p nexus-ai-core --features model
cargo clippy --offline -p nexus-ai-core --features model --all-targets -- -D warnings
.\scripts\test-ai.ps1
```

Model tests require pinned assets from `tools/nexus-model/fetch.py`. Added tests
cover privacy before tokenization, independent disclosure/private consent,
redaction, ranking/provenance, 961 Japanese byte/token budget combinations,
limits, tokenizer failure, clearing, Runtime invalidation and actual pretrained
inference from selected text against the existing oracle token.

The guest probe runs Runtime over real OS wrappers, checks Japanese text,
secret exclusion, remote refusal and cancellation, then logs:

```text
ai-probe: context PASS verified observations UTF-8 privacy budget cancellation
```

It continues with separate-service IPC, policy and capability-teardown tests.
Run evidence and integration limits are in
`.ai_collaboration/tasks/ASTRA-CONTEXT-001.json`. This is not a complete MVP or
a GUI model integration; the model remains a tiny English completion backend.
