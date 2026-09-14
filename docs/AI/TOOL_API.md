# Tool API

The core Tool enum is the version-1 operation schema. Requests have no arguments
because the sole executable tool has none. Every call passes authorize before
SystemSource::snapshot; the adapter uses nexus-user, never kernel internals.

| ID | Tool | Current outcome |
|---|---|---|
| 1 | system.info | Verified uptime_ms and service thread_id, or failure |
| 2 | fs.read | Unsupported (no directory capability provisioned) |
| 3/4 | fs.write / fs.remove | ConfirmRequired, no execution |
| 5 | terminal.execute | ConfirmRequired, no execution |
| 6 | process.stop | ConfirmRequired, no execution |
| 7 | settings.write | Privileged, no execution |
| 8 | network.connect | ConfirmRequired, no execution |
| 9 | kernel.memory | Blocked, no execution |

Other IDs and trailing arguments fail decoding. This closed registry cannot be
expanded by a client or model. Future filesystem/process/application/window/
clipboard/notification/settings/network/search adapters require explicit schemas
and lent capabilities. Enumerated refusal cases are not implemented tools.
