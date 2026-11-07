# Native fork work

Steam's actual i386 bootstrap reaches a clone request with flags `0x1200011`,
zero child stack, zero parent-TID pointer and a child-TID pointer. These are
SIGCHLD, CLONE_CHILD_SETTID and CLONE_CHILD_CLEARTID, with no CLONE_VM. The
request needs a separate process and an independent address space. Returning
a thread here would not satisfy the request.

The earlier diagnostic before `linux_fork` was wired
(`build/steam-diagnostic-74cdfb2d63334a68a125ca01f3b60afd/serial.log`)
recorded `CUpdateUIChildProcess: failed to fork child process` and exited with
4294967192. `kernel/nexus-kernel/src/compat/linux_fork.rs` now wires native
process `fork` and `wait4` across both x86-64 and i386, alongside `AF_INET`
stream/datagram sockets and socket options (`TCP_NODELAY`, `SO_ERROR`,
`SO_RCVTIMEO`, `SO_SNDTIMEO`, `SO_SNDBUF`, `SO_RCVBUF`, `SO_REUSEADDR`,
`SO_KEEPALIVE`, `SO_LINGER`, `IP_TOS`, `IP_MTU_DISCOVER`). The official Steam
bootstrap now forks its update-UI child process, downloads the client manifest
over HTTP, reaps the child, and exits cleanly (`status=0`). Full Steam client
sign-in and physical GPU acceleration remain unverified.

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

`kernel/nexus-kernel/src/compat/linux_fork.rs` now completes the Linux fork and
`wait4` path on top of `address_snapshot`:

- Clones descriptor tables with identical handle numbers and rights, preserving
  shared open file cursors and `O_NONBLOCK` state while allowing independent
  `close` and `FD_CLOEXEC` flags.
- Inherits `mmap` placement state, executable metadata, current working
  directory, signal actions/masks, x86-64/i386 TLS state, and `MemoryObject`
  shared mappings (`MAP_SHARED` `memfd`/surface views) alongside private pages.
- Starts the calling thread in the child process returning `0` while returning
  the child PID to the parent, applying `CLONE_CHILD_SETTID` and
  `CLONE_CHILD_CLEARTID` to the child's address space.
- Tracks parent/child relationships, exit status and `wait4`/`WNOHANG` reaping,
  verified by real x86-64 and i386 libc `fork`/`waitpid` probes and the official
  Steam bootstrap. Full i386 signal delivery, Steam sign-in and physical NVIDIA
  acceleration remain separate incomplete requirements.
