# Context collection

The implemented observation consists of uptime_ms and the service's thread_id,
read via real OS wrappers. The verifier repeats the observation and checks
monotonicity, deadline and identity. This establishes internal consistency, not
an independent hardware measurement. Global RAM/CPU utilization/process count
are unavailable through the current user API and are not synthesized.

Future context sources must be typed and carry origin, collection time, scope,
sensitivity and estimated size. Pipeline: collect only authorized sources,
filter privacy/scope, rank by relevance and recency, compress under a budget,
then inject while preserving provenance. nexus-index is suitable for deterministic
retrieval; its n-gram vectors must not be presented as learned model embeddings.
No model context is transmitted in this slice.
