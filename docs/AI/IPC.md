# AI IPC version 1

Transport: existing nexus-user send/receive and waitsets. One spawned service,
one parent channel (handle 1), one session (1). No service namespace or authority
lookup. Request/reply only, no unsolicited streaming. No inbound handles allowed.

All integers are little endian; exact lengths and zero reserved bytes required.

| Offset | Request (24 bytes) | Response (40 bytes) |
|---|---|---|
| 0..4 | NAI1 | NAR1 |
| 4..6 | u16 Tool ID | u16 Status |
| 6..8 | zero reserved | zero reserved |
| 8..16 | u64 session | u64 session |
| 16..24 | u64 request ID | u64 request ID |
| 24..32 | absent | u64 uptime_ms |
| 32..40 | absent | u64 service thread_id |

Status IDs: 0 Verified, 1 Blocked, 2 ConfirmRequired, 3 Privileged,
4 Unsupported, 5 Invalid, 6 Expired, 7 Exhausted, 8 Failed, 9 Cancelled.
Non-success responses carry zero observation. Malformed frames reply Invalid
with zero correlation IDs. Unknown versions/operations are rejected.

Runtime IDs must strictly increase; denials consume the ID. Session numbers are
not authentication: possession of the parent endpoint is the boundary. Service
limits total messages to 64, idle waits to 60 seconds, and closes on peer loss or
reply backpressure. There is no busy retry on full queues. An unclosable incoming
capability causes process exit rather than retained authority.
Future streaming should use explicitly requested bounded chunks and watch-ready
callbacks; large data may use separately negotiated shared memory.
