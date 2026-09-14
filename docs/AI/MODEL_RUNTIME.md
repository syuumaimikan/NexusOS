# Model boundary

Status: no provider/backend connected. This release performs deterministic OS
tool execution and does not generate conversational or model responses.

A future ModelProvider supplies advertised capabilities, bounded generation,
pull-stream continuation/cancel, structured tool proposals and optional embed/
vision. A ModelSession owns request correlation and token/resource budgets.
Unsupported capabilities return unavailable. Routing chooses among registered
real providers by supported modality, privacy/egress policy and resource budget;
no fallback may send data externally without the same authorization.

Local inference needs real weights, tokenizer, loader, quantization/KV/batching
and bounded memory, plus platform FPU/SIMD context support for conventional
backends. GPU/NPU are not currently available inference APIs. Remote inference
needs TLS authentication and secret isolation; current HTTP transport is not
sufficient. No dummy provider or successful empty model response is supplied.
