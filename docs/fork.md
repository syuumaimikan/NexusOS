# Native fork work

Steam's actual i386 bootstrap reaches a clone request with flags `0x1200011`,
zero child stack, zero parent-TID pointer and a child-TID pointer. These are
SIGCHLD, CLONE_CHILD_SETTID and CLONE_CHILD_CLEARTID, with no CLONE_VM. The
request needs a separate process and an independent address space. Returning
a thread here would not satisfy the request.

The current authoritative diagnostic is
`build/steam-diagnostic-74cdfb2d63334a68a125ca01f3b60afd/serial.log`:
the translated bootstrap still reports `CUpdateUIChildProcess: failed to fork
child process`, and exits with 4294967192. Its two earlier socket option calls
are level 6, option 1 (TCP_NODELAY), on local stream descriptors. Those calls
remain unsupported; they are not evidence that implementing options alone
would start the update UI. Steam sign-in remains unverified.

`kernel/nexus-kernel/src/memory/address_snapshot.rs` now implements an eager
copy of private 4 KiB user mappings at the same virtual addresses in a new
AddressSpace. It preserves leaf permissions and guard bytes, shares the
existing kernel upper-half tables, preflights a caller-supplied page budget,
and frees partial copies after allocation failure. Source pages/PTEs stay
unchanged. This is a native page-table operation, not a Linux host or VM
service. It is not copy-on-write and does not schedule a child.

The caller must exclude concurrent writers/mappers for the duration of the
copy. Shared, huge, restricted intermediate tables and unrecognised supervisor mappings are rejected before
allocation; shared mappings require explicit inheritance of their MemoryObject
ownership before fork can support them. This matches the separation between
private process contents and shared mappings described in the upstream
[Linux man-pages](https://www.kernel.org/pub/linux/docs/man-pages/book/man-pages-6.13.pdf).

The kernel self-test checks all bytes of four real pages at sparse 32-bit and
64-bit addresses, read-only/executable/guard permissions, different physical
frames, independent writes/unmapping, preserved holes/kernel mappings, child
data after source teardown, refusal of an insufficient page budget and shared
ownership, and allocation failure after two copied pages. It requires no
leaked frames. The runtime acceptance script requires this test's marker.

Verification on 2026-10-14: the native test passes in 4 ms in
`build/runtime-test-0833b2439f5a45e7a4e0fe0b032ef721/serial.log`. The complete
Windows PowerShell runtime/UI run also passes (libc64/i386, IPC/filesystem,
CPU Vulkan, Mesa, native window controls and upstream Wayland):
`build/address-snapshot-acceptance.log`.

Before wiring the Linux fork entry point, the remaining work includes:

- Clone descriptor tables with the same numbers and rights, preserving shared
  open descriptions/cursors while allowing independent close and CLOEXEC flags.
- Inherit mmap/brk placement state, executable metadata, actual cwd, signal/TLS
  state and all owners backing shared mappings; define source VM synchronisation.
- Start only the calling thread in the new process, returning zero to it and
  the child PID to its parent. Apply CHILD_SETTID/CLEARTID to the child's memory.
- Record parent/child relationships and completion, implement waitpid/wait4
  status/reaping and orphan cleanup, and preserve cancellation/exit semantics.
- Prove all of these using actual libc children on NexusOS, then rerun the
  official Steam bootstrap. i386 exec, signals, desktop services and physical
  NVIDIA acceleration remain separate incomplete requirements.
