# Real libc, Vulkan, Mesa OpenGL and Wayland on the NexusOS kernel

Verified on 2026-10-13 in QEMU with the NexusOS kernel. WSL executes only the
upstream Wayland protocol scanner during the build. Linux is not the guest OS.

The 2026-10-14 continuation started with no `build/` directory. Earlier
artifact paths below are historical records, not files available in this
rebuilt cache. The source changes remained, and pinned runtimes are rebuilt
and revalidated for the current filesystem/device/IPC work.

The current rebuilt development image is `build/steam-symlink-image` (8 GiB).
It contains all pinned libc/Wayland/Mesa/SwiftShader runtimes and the production
kernel. Normal init boots to the desktop with the saved `nexus`/`password`
development setup; `build/steam-symlink-image/serial.log` records boot 2 and the
shell marker, with no new setup wizard or kernel fault. Run:

```powershell
./scripts/run.ps1 -BuildDirectory build/steam-symlink-image
```

It uses QEMU's separate ESP directory. This is not a verified physical USB
boot/install image, an installed Steam client, or evidence of RTX rendering.

## Build and embed

```powershell
./scripts/build.ps1 -WithWaylandRuntime
./scripts/test-posix-runtime.ps1 -WithWayland
./scripts/build.ps1 -WithWaylandRuntime -WithMesaRuntime -SkipDiskImage
./scripts/test-posix-runtime.ps1 -WithWayland -WithMesa -DeepFilesystemChecks -Timeout 300
./scripts/build.ps1 -WithWaylandRuntime -WithMesaRuntime -WithGlibcRuntime -WithVulkanRuntime -SkipDiskImage
./scripts/test-posix-runtime.ps1 -WithWayland -WithMesa -WithGlibc -WithVulkan -SkipRuntimeBuild
./scripts/build.ps1 -WithWaylandRuntime -WithMesaRuntime -WithGlibc32Runtime -WithVulkanRuntime -SkipDiskImage
./scripts/test-posix-runtime.ps1 -WithGlibc32 -SkipRuntimeBuild
```

For libc alone, use `build.ps1 -WithPosixRuntime`. The first build downloads
Zig 0.15.2 and fixed Alpine v3.22 packages, checks their recorded SHA256
digests, and caches them under `build/toolchains` and `build/runtime`.
PowerShell, Python, QEMU and the repository Rust toolchain are required.
Wayland bindings additionally require the existing Ubuntu WSL distribution
(`build-wayland-runtime.ps1 -WslDistribution` can select another distribution).
No distribution packages or configuration are changed. Cached runtime tests
can run with `test-posix-runtime.ps1 -SkipRuntimeBuild -WithWayland`.

Mesa is cross-built from the pinned 23.3.6 source release using workspace-local
Meson, Ninja and Python dependencies, Windows flex/bison and the installed
LLVM archive/strip tools (`C:/Program Files/LLVM/bin`). The driver is softpipe;
LLVM, GLX, EGL, GBM and Vulkan drivers are disabled. Three Meson build rules are
adjusted for a Windows-to-Linux build (flex compatibility flags and version
script argument syntax); the renderer/compiler code is unchanged. A regular
GNU archive wrapper works around Zig's lack of thin-archive support. Debug
symbols are stripped from the staged library, retaining dynamic symbols.

`-SkipDiskImage` stages artifacts without replacing the existing persistent
disk. The ordinary build recreates a stale disk, including its stored settings.
An isolated ready-to-boot image from this implementation is under
`build/graphics-image`; run it with:

```powershell
./scripts/run.ps1 -BuildDirectory build/graphics-image
```

The current modern desktop build, GNU libc and CPU Vulkan are also staged in
the independent 8 GiB `build/modern-image` image:

```powershell
./scripts/run.ps1 -BuildDirectory build/modern-image
```

The i386 runtime and modern desktop are also in `build/compat32-image`, an
independent 8 GiB image booted with normal init. Run it with
`./scripts/run.ps1 -BuildDirectory build/compat32-image`. Its test account is
`nexus` with password `password`; it is an isolated development image.

The earlier semaphore/eventfd kernel and all runtime libraries are staged in
the separate 8 GiB `build/steam-ipc-image` image. Its normal desktop setup is
saved with the same development account. Run it with
`./scripts/run.ps1 -BuildDirectory build/steam-ipc-image`.
Normal init also reboots successfully with the saved settings; the boot log is
`build/steam-ipc-image/serial.log`. This is a QEMU development image, not proof
of physical RTX 5060 rendering.

The earlier executable/proc/getdents64/cwd kernel was staged in the independent
8 GiB `build/steam-proc-image` development image. Setup was saved with the same
`nexus`/`password` development account, and normal init reboots to the desktop:

```powershell
./scripts/run.ps1 -BuildDirectory build/steam-proc-image
```

Boot evidence is `build/steam-proc-image/serial.log`. Its staged kernel matches
the final focused runtime acceptance kernel. The image uses QEMU's separate
ESP directory and is not a verified physical RTX 5060 boot/install image.
The genuine Steam diagnostic is separate; this desktop image does not provide
a working Steam client or a verified browser-to-Steam installation flow.

The libraries are installed into the Linux root by `BIN/LINUX.LST` at boot:

| Library | Guest path |
| --- | --- |
| musl 1.2.5-r12 | `/lib/ld-musl-x86_64.so.1`, libc aliases |
| Wayland 1.23.1-r3 | `/usr/lib/libwayland-client.so.0` |
| libffi 3.4.8-r0 | `/usr/lib/libffi.so.8` |
| Mesa OSMesa 23.3.6 (optional) | `/usr/lib/libOSMesa.so.8` |
| Debian glibc 2.36 (optional) | `/lib64/ld-linux-x86-64.so.2`, `/lib/x86_64-linux-gnu/` |
| Debian i386 glibc 2.36 (optional) | `/lib/ld-linux.so.2`, `/lib/i386-linux-gnu/` |
| SwiftShader Vulkan ICD (optional) | `/usr/lib/libvk_swiftshader.so` |
| Runtime notices | `/usr/share/nexus-runtime/NOTICE` |

F5 starts the existing limited Wayland server. With `-WithWaylandRuntime`, F6
starts the new C client linked against the unmodified upstream library. The
default build still stages the older Rust protocol fixture. The C client's
xdg-shell bindings are generated from the upstream XML with wayland-scanner.
This server resizes and closes the test window automatically after key input.
With both runtime switches, F6 uses the Mesa-linked client instead. It compiles
real GLSL and draws near/far triangles into the actual shared Wayland buffer,
using OSMesa BGRA output to match `WL_SHM_FORMAT_XRGB8888`.

## Verified behavior

The test boots a unique 256 MiB disposable disk, replaces only its init with a
completion-checking probe, and keeps logs and screenshots in
`build/runtime-test-<GUID>/`. The normal disk, ESP and settings are untouched.

The current combined graphics run is
`build/runtime-test-5394c5fd42f0449197ef5894d49767a7/serial.log`.
Musl, x86-64/i386 GNU libc, Mesa and Vulkan pass together. With `-WithUIControls -WithWayland`,
independent screenshots also check maximise/restore/minimise, Dock restore and
client-approved close in the actual desktop. `desktop.png` and `wayland.png`
show the floating Dock, rounded windows, shadows, title controls and wallpaper.
`cargo test --offline -p nexus-abi -p nexus-ui -p nexus-look` passes 45 host tests.

- Real musl `malloc`, file I/O, positional reads, clocks and sleep.
- Native shared mappings retain their memory objects after handle close/transfer
  and after another alias is unmapped. Allocating/writing a different object
  does not alter the remaining alias. This also fixes normal init's shared-page
  handshake when the child exits before its parent reads back the page.
- Distinct nonzero file identities in `fstat`, required for loader deduplication.
- Guard reservations with `PROT_NONE`, kernel rejection of guard-buffer I/O,
  and permission restoration preserving bytes.
- Real `pthread_create`/`pthread_join`, independent TLS, and 80 immediately
  exiting threads, including reuse beyond the concurrent thread limit.
- 256 concurrent pipe/poll round trips with both musl and GNU libc. Poll/epoll
  recheck requested readiness after watcher registration to avoid losing a write
  between the first check and blocking. Read-only interests do not busy-loop on
  sockets that are writable.
- Futex waits without the private flag; wake with an unchanged word; zero-count
  wake; two aliases of a memfd sharing the same futex; invalid and expired
  deadlines. Protecting/unmapping one alias leaves the other usable.
- The real musl dynamic interpreter, `dlopen`, `dlsym`, and a shared library
  accessing actual TLS (the TLS value is volatile to prevent constant folding).
- Upstream libwayland/libffi registry dispatch, xdg-shell negotiation, descriptor
  transfer, shared buffers, two frame callbacks and buffer releases, keyboard
  focus/input and close. An external pixel check confirms all three client
  bands in order, at the resized width of 240 pixels.
- Actual OSMesa context creation, `GL_RENDERER=softpipe`, depth occlusion and
  GLSL compilation/linking/rendering on NexusOS. The driver reports OpenGL 3.3
  compatibility; this is not a complete OpenGL conformance result.
- A Mesa-rendered triangle visibly reaches the Wayland window. The external
  screenshot test counts 1,920 foreground triangle pixels at the resized
  viewport and verifies that the identical far triangle is occluded.

Full libc/Mesa/Wayland and deep filesystem checks passed together in
`build/runtime-test-57dfc91c863442a2af16deba22cca6b1/serial.log`; the visible
result is `wayland.png` in the same directory. Earlier tests exposed a build
archive incompatibility, incorrect framebuffer initialization and an incorrect
screenshot origin beneath the native title bar. All were corrected before the
final acceptance run; success is checked from process status and actual pixels.
The separate 8 GiB graphics image also boots with the normal init and without
deep filesystem checks; `build/graphics-image/serial.log` records the successful
shared-memory handshake and the normal Linux fixture executions.

NexusFS now supports 64 MiB files using a double-indirect root stored in four
previously reserved inode bytes. Version-two volumes remain readable. The first
large write journals a version-three superblock with its new index/inode;
older kernels refuse that version rather than misidentifying its live blocks.
Boot runtime imports stream in 64 KiB chunks to avoid whole-library kernel
allocations. Failed imports remove the partial file so the next boot retries.
Large files should be read via offset/chunk APIs rather than whole-file reads
because the kernel heap is smaller than this file limit.

Deep checks stream over 6 MiB, remount and check live index ownership, compare
all bytes, shrink/free index blocks, verify zero-filled gaps on reused blocks,
and exercise the existing journal crash/abandon tests. They run only on the
test's fresh disposable disk. Metadata limits can still reject a transaction
on a sufficiently fragmented volume.

The kernel records clear-tid metadata and writes clone identifiers before the
new thread becomes runnable. Futex keys are process/virtual-word for private
backing, or backing physical words for shared memory objects. Only
`FUTEX_BITSET_MATCH_ANY` is supported for bitset operations; selective bitsets,
PI and requeue are still unsupported. Absolute realtime WAIT_BITSET deadlines
are supported, including the untimed form used by GNU pthread joins. This is a
focused compatibility test, not the POSIX or libc conformance suite.

## GNU libc and upstream Vulkan

Pinned Debian `2.36-9+deb12u14` x86-64 loader and libraries pass real allocation,
file, clock, pthread/TLS, repeated join and dlopen tests. The unmodified Debian
i386 loader and libraries also execute on the custom kernel. Use
`-WithGlibc32Runtime` to build/stage them and `test-posix-runtime.ps1 -WithGlibc32`
to boot all three i386 probes on a fresh disposable disk.

The i386 ELF loader supports fixed executables and PIE, PT_INTERP and AT_BASE.
It maps a 512 KiB initial stack, matching the deeper stack use of the Debian
i386 glibc startup and its `clone3` calls. `/proc/self/cmdline` and `environ`
refer to separate contiguous argv/environment strings, including in-place edits.
The compatibility layer translates packed stat64, iovecs, legacy timespecs,
time64, clone register order and CLONE_SETTLS user_desc. Each thread carries
three TLS descriptors, FS/GS selectors and bases across scheduling and CPU
migration. The kernel preserves its own GS base throughout selector reloads.
`exit` ends only the calling thread; `exit_group` ends the process.

Evidence: `build/runtime-test-6a77d3f758734f80a1ce527bfacbf746/serial.log`.
The probes check allocation, file/clock operations, distinct stat64/statx
identities, get/set_thread_area allocation/exhaustion and invalid descriptors,
pthread/join, shared futex aliases/deadlines, 80 immediate thread exits,
four simultaneous TLS owners across 4,096 scheduler yields, 256 pipe/poll
round trips, dlopen/dlsym and shared-library TLS. Acceptance requires the
actual test messages as well as successful process completion. Loader host
tests also check ELF32 rebasing, code/BSS preservation and rejection before
allocation for overflowing images or invalid interpreter paths.
Epoll also checks pipe readiness and all 64 bits of the packed event's data.
The descriptor-reload window retains a recoverable kernel GS base for NMI;
interrupt entry handles this ring-zero transition as well as ring-three entry.

i386 signal handlers/frames, robust mutex cleanup, fork/exec process semantics
and the complete socket ABI are still incomplete. The tested glibc pthread
paths tolerate ENOSYS for signal setup and robust-list registration; this does
not verify signal delivery, cancellation or robust mutexes.

SwiftShader commit `0f87bb2d3742c2992fd1a7dcb2f051eebf79299c` is cross-built
with its Subzero shader compiler. The probe negotiates its genuine ICD interface,
creates a Vulkan device, buffer, descriptor set and compute pipeline, compiles
SPIR-V, submits a command buffer and checks the shader's write in host memory.
This is CPU Vulkan. There is no Vulkan loader discovery, window swapchain or
physical GPU submission yet. Evidence is in
`build/runtime-test-68d21023fadd464e9e05f9063d9ceeb7/serial.log`.

## System V semaphores and counted notifications

The native kernel now implements semaphore sets, named keys, semctl metadata,
atomic operation vectors, zero/decrease waits, relative deadlines, removal
wakes and SEM_UNDO. CLONE_SYSVSEM shares undo ownership; an independent thread
applies its list on exit, before its clear-child-TID join notification. SETVAL
and SETALL clear the corresponding undo entries. i386 supports the original
ipc multiplexer, its old/IPC64 structures, direct semget/semctl and time64
semtimedop. Message queues and System V shared memory remain separate,
unimplemented services. Limits are explicit: 128 sets, 256 elements per set,
4096 elements total and 128 operations per transaction. Waiters re-evaluate
their transactions after a wake; strict Linux FIFO ordering is not implemented.

eventfd and eventfd2 use a native counter object held by ordinary Nexus handles.
They support 64-bit accumulation, EFD_SEMAPHORE reads, blocking and nonblocking
overflow handling, and poll/epoll readiness. Duplicates share the counter and
O_NONBLOCK; FD_CLOEXEC is remembered per descriptor. General exec descriptor
inheritance/CLOEXEC enforcement remains part of the unfinished exec work.

The real musl, x86-64 GNU and i386 GNU pthread probes verify transaction
rollback, intermediate undo overflow, metadata, invalid output mappings,
deadlines, stale IDs, wait counts/removal and shared undo ownership. Both GNU
probes also verify independent undo ownership through the genuine clone
wrapper. A metadata reader races 80 clones to check scheduler/registry lock
ordering. Counter probes check accumulation, limits, buffer sizes/protection,
shared flags, 128 concurrent epoll round trips and a blocked overflow writer.
The combined libc, Mesa, Vulkan, Wayland and desktop-control run passes:
`build/runtime-test-9f4a587ff164407686fdf587330515c6/serial.log`.

ABI references are the upstream Linux
[semaphore implementation](https://github.com/torvalds/linux/blob/master/ipc/sem.c),
[i386 IPC entry points](https://github.com/torvalds/linux/blob/master/ipc/syscall.c)
and [eventfd implementation](https://github.com/torvalds/linux/blob/master/fs/eventfd.c).

## Steam and hardware graphics remain incomplete

These changes establish x86-64 musl, x86-64/i386 GNU runtimes and a real Wayland client.
They do not provide the full Steam runtime, a shell/bootstrap environment,
complete process/network/audio services, Xwayland, a Vulkan loader and swapchain,
or GPU rendering. Mesa softpipe is software OpenGL. Existing native CPU 3D drawing and
SPIR-V interpretation are separate native features and are not Vulkan. The browser-download → install → Steam
sign-in acceptance test has not passed.

See [the compatibility gates](steam-graphics.md) for the remaining work.

## Executable identity and current-process bytes

The loader records the actual executable node and the argument/environment
memory ranges before starting its first thread. `/proc/self/exe` (also the
current numeric PID and `thread-self` forms) resolves to that executable, not
to its interpreter or argv[0]. readlink/readlinkat implement short buffers,
no appended NUL, and mapped/writable output checks. Opening the link reads the
real ELF node; stat follows it and lstat/AT_SYMLINK_NOFOLLOW describe the link.

`/proc/self/cmdline` and `environ` are read-only native process-information
objects in the ordinary handle table. Their bytes come from the current
process's mapped string ranges, including subsequent in-place edits. Dups
share the cursor and nonblocking status; pread does not change that cursor.
fstat/stat64/statx describe stable virtual identities with zero st_size.
Successful x86-64 execve refreshes image metadata, including descriptors
opened before the replacement. The i386 getdents64 path also uses real
filesystem directory entries and inode identity.

The libc verification launches the genuine probes from their installed Linux
paths. A directly FAT-launched binary has no Linux executable node and gets
ENOENT for its exe link, rather than an invented path. These interfaces are
limited to the current process. Proc directory enumeration, other processes,
fd/maps links, PR_SET_MM, setproctitle extension beyond the initial argv range,
and rename/deletion updates to the recorded executable pathname remain absent.
Close-on-exec is recorded but is not yet enforced by execve.

The graphics integration run verifies all nine basic/pthread/dynamic libc probe
variants, including real directory enumeration, plus both x86-64 execve
replacement tests. It also passes the existing semaphore/eventfd tests, CPU
Vulkan SPIR-V and Mesa GLSL/depth tests, real Wayland transport and visible
window resize, and pixel-checked modern decoration/Dock controls:
`build/runtime-test-a8c5ca94a3784d6984f0a6b355ac26e4/serial.log`.
The saved screenshot is `wayland.png` in that directory. These are QEMU and
CPU-rendering results; no physical NVIDIA submission occurred.
The subsequent buffer/rights hardening run passes the same libc variants,
including read-only directory-output rejection at both widths:
`build/runtime-test-967c4e560c5e42b3a44e0c343e1d3678/serial.log`.

Reference interfaces: [proc executable link](https://man7.org/linux/man-pages/man5/proc_pid_exe.5.html)
and [Linux process argument reads](https://github.com/torvalds/linux/blob/master/fs/proc/base.c).

## Working directory and relative paths

The native process holds an actual directory node, shared by its threads.
Both x86-64 and i386 implement chdir/fchdir/getcwd. Relative paths begin at
that node or an open directory FD; absolute paths begin at the Linux root and
ignore dirfd. The resolver walks every parent component: `missing/..` remains
ENOENT, `file/..` remains ENOTDIR, and parents of the root stay at the root.
openat, stat/stat64/statx, access, mkdirat, unlinkat, readlinkat and renameat
use this resolver. Unlink and rmdir check file/directory kind separately.
Mutations of `.` or `..` are refused instead of deleting their resolved target.

NexusFS supplies live directory ancestry while holding its volume lock. Names
therefore reflect directory renames; open descriptors and cwd keep their native
inode identity. Ancestor nodes are pinned before releasing the volume lock.
getcwd checks writable output and returns the byte count including NUL. An
unlinked cwd reports ENOENT; an absolute chdir can recover. x86-64 execve
retains cwd and pins the exact executable node read before memory replacement,
including relative executable paths; argv[0] is independent of its proc identity.

The focused libc run passed all nine basic/thread/dynamic variants and both
x86-64 relative-exec replacements:
`build/runtime-test-397c7610b5054144b1cc9bcba721cdf7/serial.log`.
The tests also verify directory rename, directory-FD file creation/stat/rename,
shared pthread cwd, removed-directory identity before last close, parent
traversal, small/read-only output buffers and ordinary error cases.

The final integration run also verifies fstatat/AT_EMPTY_PATH cwd identity,
directory-FD creation/stat, bad fchdir descriptors, and O_CREAT/O_DIRECTORY
validation across those nine variants, plus both relative exec tests. Existing
semaphore/eventfd, SwiftShader SPIR-V, Mesa GLSL/depth, upstream Wayland
resize/input/release/close and pixel-checked desktop controls all pass:
`build/runtime-test-1714b4948087428aae69198db98c57da/serial.log`.

The initial integrated run caught a compositor ordering defect: it replied
`shown` while the shared surface still awaited batched repaint. Native windows,
wallpaper and the shell now receive that reply after the batch finishes;
hidden/asleep frames are released without rendering. The final screenshot
shows the actual resized client buffer rather than the earlier frame.
This does not add GPU scanout/vblank synchronization or full Wayland presentation.

Limitations: directory ancestry currently scans the Linux namespace. Entering
an already unlinked directory FD is refused (ENOENT); private filesystem
contexts for clone without CLONE_FS are absent. Permission bits and mount
namespaces are absent. Native rename still refuses
an existing destination, including flags=0; replacement/exchange/whiteout
semantics are not implemented. i386 execve supports the tested ELF32 static
and dynamic images; broader ABI and application coverage remains incomplete.

Reference semantics: [working-directory buffers and removed directories](https://man7.org/linux/man-pages/man3/getcwd.3.html)
and [directory-relative opening](https://man7.org/linux/man-pages/man2/open.2.html).

## Persistent links, random-source descriptors and i386 stream IPC

NexusFS adds immutable symlink inodes. Link targets, parent entries and the
version-four superblock upgrade are committed in one journal transaction;
v2/v3 volumes remain readable, and older kernels refuse v4. Large-file writes
retain that version rather than downgrading it to v3. Deep checks verify
commit-before-home-write recovery, remount, a 4,095-byte target, abandoned
creation, immutability and reclaimed blocks. Native NodeSymlink/NodeReadLink
calls expose the same storage through directory capabilities; directory lists
identify links, attenuated directories cannot create them, and an unlinked
open link retains its bytes until its last close.

Both Linux widths translate symlink/symlinkat and follow relative and absolute
targets beneath the Linux root, with a 40-link limit. lstat/AT_SYMLINK_NOFOLLOW,
readlink, O_NOFOLLOW, directory-FD operations, dangling-link creation, rename
and unlink distinguish the link from its target. Removing a link does not
remove its target. Archive unpacking still skips recorded links; the full
browser/package installation workflow has not yet been completed.

`/dev/urandom` is a native random-source capability backed by the existing
RDSEED/RDRAND service. Successful reads use actual hardware-source bytes,
validate writable output, and leave output untouched if generation fails.
It has character metadata (major 1/minor 9), duplicate-shared nonblocking
status and immediate readiness on supported hardware. Per-read output is
capped at 64 KiB, with normal short reads. Hardware absence returns ENODEV;
transient generation failure returns EAGAIN. Entropy writes/mixing, a software
pool, /dev/random, seeking and positioned reads remain unsupported.

Legacy i386 socketcall decodes bounded arrays of 32-bit argument words and
translates basic UNIX stream socket, bind/listen/connect/accept, socketpair,
send and recv. Tests use genuine byte exchange, MSG_DONTWAIT, checked read-only
outputs and a four-byte socklen at the end of a page. Structures such as the
i386 msghdr/control layout, socket options, datagrams and SOCK_NONBLOCK remain
unsupported. CLOEXEC is recorded, but exec still does not enforce it.

An integrated run exposed exit_group publishing cancellation before recording
its exit code, allowing a sibling's stop to win the completion status. The
exit code is now claimed first. Dedicated musl/GNU/i386 GNU probes repeatedly
exit with code 42 while eight sibling threads are actively making syscalls;
the native parent checks the actual observed completion status.

Current verified symlink/native capability/journal and random-source tests,
all nine libc variants, and Wayland/desktop control tests are recorded in
`build/runtime-test-f871a33b87df4fb59f1c6457f29802ac/serial.log`.
The final socket/group-exit/full-renderer integration passes all nine libc
variants, all three i386 socketcall probes, 24 group-exit probes (eight per
libc family), deep link/large-file/journal checks, SwiftShader SPIR-V, Mesa
GLSL/depth, Wayland transport/resize/input/release/close, and pixel-checked
desktop controls:
`build/runtime-test-5394c5fd42f0449197ef5894d49767a7/serial.log`.
Its `wayland.png` shows the current software-rendered desktop. The normal
build and isolated 8 GiB `build/steam-symlink-image` use the production kernel
without destructive deep checks; these checks run only on disposable disks.

## Genuine Steam bootstrap diagnostics

`scripts/diagnose-steam.ps1` runs the official Valve installer version
`1:1.0.0.87`'s actual ELF32 bootstrap on a separate 256 MiB NexusOS disk.
The exact download, package and executable hashes are pinned in
`tools/nexus-runtime/fetch-steam.py` and recorded in the cache manifest.
Only named regular archive members are extracted; package scripts are never
executed on Windows. This tool does not register a working Steam launcher,
accept an agreement or verify Steam sign-in.

The bootstrap reaches its own initialisation through the genuine GNU loader.
Its first run exited with status 1 after epoll and temporary-directory errors:
`build/steam-diagnostic-0eb7c8b3615144dda9d0ddcbf0687e29/serial.log`.
The runtime filesystem now supplies `/tmp`, and i386 epoll create/control/wait
uses the actual readiness implementation. The next run proceeds further but
reports incomplete permissions, `/proc`, signal and System V IPC services,
then times out on its unavailable synchronization object:
`build/steam-diagnostic-4b37b4366bb74be09a6d604f441fe939/serial.log`.
The semaphore implementation resolves that synchronization failure. After
adding eventfd, the bootstrap also stops reporting the completion-port post-FD
and epoll-control assertions. That diagnostic reaches later startup, then exits
with status 4294967195 after failing to determine its executable path:
`build/steam-diagnostic-e9a6d0b28bea4c49b4cef4b900f6cb5d/serial.log`.
After adding genuine executable and proc metadata, Plat_GetExecutablePath no
longer fails. The next actual bootstrap exits with status 4294967194 and
reports "Couldn't chdir into the install path". Its proc reporter also asks
for flock, which remains unsupported. Named FIFO creation, working-directory
changes, permissions, i386 signal frames and additional process/runtime services
still need implementation:
`build/steam-diagnostic-52b17277c13242fcb35f8836c2fe020e/serial.log`.
The final getdents64-enabled kernel repeats this result without the i386
directory-enumeration ENOSYS messages:
`build/steam-diagnostic-fa150cbd40754a719860ed51b7dbe3ac/serial.log`.
The diagnostic records the actual exit; this is not a usable client.

With actual cwd/relative-path support, the chdir assertion and i386 chdir/rename
ENOSYS messages disappear. Steam creates its log directory and now stops at
`main.cpp (1397)`: the Steam data link `/root/.steam/steam` does not exist.
The actual exit is 4294967188, recorded in
`build/steam-diagnostic-0ab79a712f3a4677af2d23c34a42d5d2/serial.log`.
This isolated bootstrap diagnostic does not run the official launcher or
installer; the missing data link is an installation-layout requirement, while
NexusFS ordinary symlinks were not implemented in that earlier run. Named FIFOs (mknodat),
flock, faccessat2/permissions and i386 signal services remain missing. There
is no client startup or sign-in success.

The updated diagnostic creates native persistent links matching the pinned
official `steam.sh` bootstrap layout (STEAMROOT/STEAMDATA = /usr/lib/steam).
It runs the real ELF32 binary without running the full shell launcher. The
data-link assertion disappears, then `/dev/urandom` becomes its next fatal
assertion: `build/steam-diagnostic-20d2711abfc74dee9d593e534b0d6c6e/serial.log`.
The genuine random-source descriptor resolves that assertion. The bootstrap
then exits with 4294967192 at `main.cpp (1433)`, unable to initialise its update
UI's IPC socket: `build/steam-diagnostic-5ae73e56f22b425cb098adbe7825584e/serial.log`.
After adding the i386 socketcall stream path, unimplemented socket options and
the refused process-fork request appear before that same update-UI failure:
`build/steam-diagnostic-135d05b3a549460da54acd1a83d52253/serial.log`.
The decoded current diagnostic identifies two level-6/option-1 requests and
clone flags `0x1200011`; the explicitly reported fatal operation is fork:
`build/steam-diagnostic-74cdfb2d63334a68a125ca01f3b60afd/serial.log`.
Native private address-space snapshots and Linux fork/wait are implemented and
tested, as is i386 execve. The bootstrap still reaches unsupported FIFO, flock,
permission, signal, socket-option and application-specific requests. See
[native fork implementation and its remaining requirements](fork.md).
FIFO, flock, permissions, i386 signals, further process/network and desktop
services remain incomplete. None of these diagnostics proves sign-in.

The physical target is the user's RTX 5060 (`10de:2d05`). Native NVIDIA boot
discovery reads PCI/D0/BAR0 and the published Blackwell identity registers.
It does not start GSP, allocate VRAM or submit GPU work. Firmware framebuffer
scanout and QEMU virtio display are not proof of RTX acceleration. The exact
source revision and fetch digest are recorded in `fetch-nvidia.py`.
See [physical RTX 5060 implementation status](rtx-5060.md).

Earlier Steam runs showed an invalid `clone(flags=0x800600)` request, which
correctly receives `EINVAL` because `CLONE_SIGHAND` requires `CLONE_VM` and
`CLONE_THREAD`. The latest run also starts valid `clone3` workers, installs the
system PEM roots into `/etc/ssl/certs/ca-certificates.crt`, and passes its
`statvfs64` query. It reaches “Verifying installation...” before Steam exits
with `4294967294` after an illegal worker-thread termination. This still does
not prove an installed client or sign-in. The current serial evidence is
`build/steam-diagnostic-7f742494ef5d4d1e995b02659631e91a/serial.log`.

The latest `scripts/test-posix-runtime.ps1 -WithGlibc32 -Timeout 360` run passes
all musl, x86-64 glibc and i386 glibc configurations, including flock lifetime
and conflicts, `statfs`/`fstatfs` on files and console descriptors, alternate
signal-stack install/query/disable, i386 `sysinfo`, robust-list layouts,
32-bit `ugetrlimit` values for `RLIMIT_NOFILE`, `RLIMIT_CORE` and
`RLIMIT_STACK`, and `kill(pid, 0)` validation. The real i386 glibc pthread test
also passes with the reported 8 MiB stack limit:
`build/runtime-test-4f73bcde83994a3dabb50b3add94ee74/serial.log`.

The same kernel then reran the Steam bootstrap diagnostic. It still reaches
“Verifying installation...” and exits with `4294967294`; the new i386 resource
limit support does not yet resolve Steam's worker-thread termination:
`build/steam-diagnostic-bcad4c92d6d94cfd8c2e6662cb55ca45/serial.log`.

## Launch and maintenance verification (2026-10-14)

The default `build/nexus-disk.img` has now been created through `run.ps1` and
configured through the native first-run wizard. A full runtime-enabled
`build.ps1` refreshes its FAT32 program partition while preserving NexusFS.
`build/default-build-preservation.log` records the successful build. Host
regressions (`scripts/test-scripts.ps1`, also run under Windows PowerShell 5.1)
compare every protected byte, require program changes, reject a changed GPT
layout without writing, and exercise active/locked cleanup and `-WhatIf`.

`scripts/clean.ps1` selects only registered disposable runtime/Steam/graphics
test payloads, keeps two newest runs per family, and preserves logs and PNGs.
The older diagnostic paths above continue to identify their retained evidence;
their disposable disk/ESP/program copies may have been pruned. Persistent
development images and build dependencies remain available. Development changes
are committed and pushed automatically after verification, per `AGENTS.md`.

`scripts/test-run.ps1` passed the configured desktop boot from a different
working directory with spaces in the QEMU file paths, missing-marker failure,
host-port conflict diagnostics, and refusal of a second launch without clearing
the first run's log (`build/run-regressions.log`, `build/run-desktop.log`). The
GTK interactive path also booted and exited normally through the native QEMU
monitor; `build/default-desktop.png` captures its 640x480 desktop. Headless UI
pixel tests use the 1920x1200 scanout. These are QEMU/software-rendering results.

The combined Windows PowerShell 5.1 acceptance also passes all configured
libc/IPC/filesystem, Vulkan, Mesa, native UI pixel and upstream Wayland cases:
`build/maintenance-runtime-acceptance-final.log` and
`build/runtime-test-93eb7fe9ca8e40e887e9a0f24646143a/serial.log`.
Compilation of an isolated staged-source snapshot passes bootloader, kernel,
all native user programs and both Linux guest targets; its normal build script
also stages successfully. JSON's no_std portability is necessary for the native
target (15 host tests pass), and staging of the optional Gemini binary now
depends on its actual target declaration. Other local AI/browser work is not
included in this snapshot.

## Sources and notices

Package URLs, exact archive digests and extracted member names are recorded in
`tools/nexus-runtime/fetch-musl.py` and `fetch-wayland.py`. Extraction accepts
only explicitly selected regular files; archive paths and symlinks are never
written to the host filesystem. Scanner-only XML/Expat/zlib/xz libraries are
build tools and are not staged in the guest.

- [Zig releases](https://ziglang.org/download/): the cross compiler and its musl
  headers/static libraries. Its bundled musl copyright notice is retained.
- [Alpine musl](https://pkgs.alpinelinux.org/package/v3.22/main/x86_64/musl),
  [Wayland client](https://pkgs.alpinelinux.org/package/v3.22/main/x86_64/wayland-libs-client),
  [libffi](https://pkgs.alpinelinux.org/package/v3.22/main/x86_64/libffi): genuine
  shared libraries, rather than repository replacements for these APIs.
- Wayland and libffi notices are copied from their upstream release licenses
  into `tools/nexus-runtime/notices/`, then staged alongside the musl notice.
- [Mesa 23.3.6 release and published checksum](https://docs.mesa3d.org/relnotes/23.3.6.html).
  `build-mesa.py` retains upstream copyright/license headers and LLVM C++ runtime
  licenses in `mesa-notice.txt`, which is included in the guest NOTICE. This
  notice collection includes unused source components as a documented superset.
