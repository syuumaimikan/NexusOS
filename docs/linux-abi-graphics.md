# A custom kernel, the Linux userspace ABI, and Mesa on top

```
probe: have getpid (39)
probe: MISSING mremap (25) -- realloc of a mapping
probe: MISSING madvise (28) -- malloc returning memory
probe: MISSING fcntl (72) -- close-on-exec, non-blocking
probe: MISSING nanosleep (35) -- sleeping at all
...
probe: 1 of 24 present, 23 missing
```

`build/probe-test.log`, driven by `scripts/test-probe.ps1`. The whole list is in
that log; the point of this page is that **it is a list the machine printed and
not a list anybody remembered.**

## The decision

Keep the kernel. Implement the Linux *userspace* ABI. Bring software built
elsewhere — a C library, then Mesa — and run it on top.

This is not a new idea and it is not a fork. Fuchsia does it with Starnix,
Managarm does it, gVisor does it, WSL1 did it, and FreeBSD has done it for
decades. The kernel stays written here; Linux binaries are *guests* of an
interface, and an interface is a document rather than a codebase.

It also settles a question this repository had been avoiding. "Not a Linux fork"
and "install Mesa" look like they contradict each other. They do not: what is
being adopted is an ABI, and Mesa is then an ordinary program that runs on this
machine the way `demo.deb` was meant to.

## Where Mesa can be made to fit, and where it cannot

Mesa is not one thing, and most of what makes it enormous is optional.

| what | needs | verdict |
|---|---|---|
| **OSMesa + softpipe** | a C library, and nothing else | **the first target** |
| llvmpipe | the above, plus LLVM | later, if ever |
| virgl | the above, plus DRM ioctls on virtio-gpu | the way to make it fast |
| radv / anv / nvk | libdrm, and a kernel DRM driver per vendor | not here |

**OSMesa is the one that matters.** It is Mesa rendering off-screen into a
buffer the caller supplies — no DRM, no X11, no Wayland, no LLVM, and no kernel
graphics driver of any kind. A compositor here already hands a client a shared
buffer and asks it to draw into it. That is precisely OSMesa's shape.

So the shortest path from here to real OpenGL runs through a C library and
nothing else. Not through a GPU driver, not through Vulkan, and not through
Wayland.

Mesa is not built on this machine. It is cross-compiled on the build host, the
way `tools/nexus-guest` already cross-compiles Linux programs with no Linux
toolchain in sight.

## What is actually missing, measured

`tools/nexus-probe` is a Linux program that makes each call with arguments
chosen to be harmless and looks only at whether the answer is `ENOSYS`. Linux
returns that for a call a kernel does not implement and for nothing else, so it
separates "missing" from "present and refused the arguments" exactly.

Nothing it does changes anything. The trick is a file descriptor of `-1`: every
call taking one answers `EBADF` when it is implemented, whatever else it would
have done. Where there is no descriptor, a null pointer earns `EFAULT` for the
same reason.

**`getpid` is on the list as a control.** If it ever comes back missing, the
probe is wrong rather than the kernel — which is the failure a program like this
is most likely to have and least likely to notice. `scripts/test-probe.ps1`
checks for it rather than leaving it to a reader.

Of 24 calls asked about, 23 are missing. Grouped by what needs them:

- **A C library's start-up and allocator**: `mremap`, `madvise`, `getrlimit`,
  `prlimit64`, `prctl`, `sysinfo`, `membarrier`
- **Files, including `/proc` and a shader cache**: `fcntl`, `fsync`, `readv`,
  `pwrite64`, `readlink`, `readlinkat`, `chdir`, `statfs`, `statx`, `lstat`
- **Time and waiting**: `nanosleep`, `clock_nanosleep`, `gettimeofday`, `time`
- **Processes**: `wait4`
- **Waiting on many at once**: `select`

`fcntl` and `nanosleep` are the two that nothing gets past. The rest have a
plausible order.

## What this list is not

**It is not the whole gap.** It is the gap in the calls that were asked about,
and those were chosen from what a C library needs to start and what a software
rasteriser needs to run. A call nobody thought to put on the list is a call that
is still missing and is not reported.

The way to find those is the way this list was found: run the real thing and
read what it says. The kernel already answers an unimplemented call with
`ENOSYS` and says so in the log, so the first musl binary to start will name its
own missing calls, and the first Mesa to load will name the next set. **The
probe is a way to get most of the list before that, not a substitute for it.**

[steam-graphics.md](steam-graphics.md) holds the other half: what Steam itself
would need, and why the Chromium in its store front is a wall that none of the
above touches.

## The order of work

1. **The 23 calls above.** Finite, and each one is small.
2. **A static musl binary that starts.** The first real C library on this
   machine, and the thing that produces the next list.
3. **OSMesa and softpipe, cross-compiled.** Real OpenGL, in software.
4. **A bridge**: the compositor's surface is the buffer OSMesa draws into.
5. **virgl and the virtio-gpu DRM ioctls**, which is where speed comes from: the
   guest encodes a command stream and the *host* does the drawing. The
   virtio-gpu driver here already exists for 2D, so 3D contexts are an extension
   of it rather than a new device.

Steps 1 and 2 are an afternoon each. Step 3 is a build problem rather than a
programming one. Step 5 is the large one, and it is the only step that needs a
line of graphics code in this kernel.

## What it will still not be

Fast. Every step up to 4 computes pixels on the processor, and
[three-d.md](three-d.md) has the measurement of what that costs: 960×582 at
about a frame a second for eight flat triangles. OpenGL will not make that
quicker; it will make it *correct and standard*, which is what lets software
written elsewhere run at all.

Step 5 is the one that changes the speed, because after it the drawing is not
happening here.
