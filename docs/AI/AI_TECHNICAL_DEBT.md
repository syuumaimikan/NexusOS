# Technical debt and integration constraints

* Current user ABI has no global memory-statistics/process enumeration API;
  system.info must report only fields it can observe.
* No TLS/secret store: remote LLM support is blocked on authenticated transport
  and credential isolation. Do not substitute plaintext requests.
* No local inference engine/model/tokenizer or accelerated compute driver;
  soft-float user target is an additional backend constraint.
* No guest compiler/standard pipes/environment identified: coding-agent build
  execution cannot be honestly claimed yet.
* CPU/memory/process quotas and a trusted approval broker are not complete.
  Keep mutation and arbitrary execution disabled until these boundaries exist.
* Initial service is single-session and request/reply, not a streamed LLM,
  durable task DAG, assistant panel, memory database or autonomous planner.
* Core deadlines are cooperative around synchronous read-only syscalls;
  preemption of a hanging future tool requires a supervised worker process.
* Shared state filenames from the directive are absent; older audits/README
  should be reconciled by their owner rather than silently used as current truth.
* Windows-oriented build scripts and floating nightly limit reproducibility.
  Keep AI test artifacts separate from normal staged disks and user files.
