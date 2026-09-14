# Memory and retention

The initial runtime retains only its last request ID, remaining budget, state
and latest numeric audit record. No conversation, file content, model output or
personal memory is persisted. The service allocates no heap.

Future working/short-term/long-term/episodic/semantic/preference/task/system memory
must use one envelope: source, timestamp, confidence, scope, retention deadline
and sensitivity. Long-term writes require user opt-in and a dedicated directory
capability. Retrieval must exclude expired or out-of-scope records before ranking;
deletion must remove derived indexes as well. Never store credentials as memory.
NexusFS persistence/recovery and expiry tests are prerequisite acceptance gates.
No memory database or retention implementation is claimed by this slice.
